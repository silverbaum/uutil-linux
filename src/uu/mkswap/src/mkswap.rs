// This file is part of the uutils util-linux package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

use clap::{crate_version, Arg, ArgAction, ArgMatches, Command};
use uucore::error::{UResult, USimpleError};
use uucore::{format_usage, help_about, help_usage};

const ABOUT: &str = help_about!("mkswap.md");
const USAGE: &str = help_usage!("mkswap.md");

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use core::ffi::{c_char, c_uchar, c_ulong};
    use linux_raw_sys::{
        general::FS_NOCOW_FL,
        ioctl::{BLKGETSIZE64, FS_IOC_GETFLAGS, FS_IOC_SETFLAGS},
    };
    use nix::{
        fcntl::{fallocate, FallocateFlags},
        sys::statfs::{fstatfs, BTRFS_SUPER_MAGIC},
    };

    use std::{
        fmt::{Debug, Display},
        fs::{File, OpenOptions, Permissions},
        io::{self, BufRead, BufReader, Cursor, Seek, SeekFrom, Write},
        mem::{offset_of, size_of},
        os::{
            fd::{AsFd, AsRawFd},
            linux::fs::MetadataExt,
            unix::fs::{FileExt, FileTypeExt, PermissionsExt},
        },
        path::Path,
        str::FromStr,
    };
    use uucore::{
        error::UUsageError,
        libc::{geteuid, ioctl, sysconf, _SC_PAGESIZE, _SC_PAGE_SIZE},
    };
    use uuid::Uuid;

    #[derive(Debug)]
    enum MkswapError {
        TooLongLabel,
        TooFewPages { pages: u32 },
        MaxBadPagesExceeded { max_badpages: usize },
        SwapAreaTooSmall { min_swapsize: u64 },
    }

    impl Display for MkswapError {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            match self {
                Self::TooLongLabel => write!(
                    f,
                    "Label is too long, maximum size is {SWAP_LABEL_LENGTH} characters"
                ),
                Self::TooFewPages { pages } => write!(
                    f,
                    "Too few pages for a swap area ({pages}), minimum is {MIN_SWAP_PAGES}"
                ),
                Self::MaxBadPagesExceeded { max_badpages } => {
                    write!(f, "Too many bad pages: {max_badpages}")
                }
                Self::SwapAreaTooSmall { min_swapsize } => write!(
                    f,
                    "error: swap area needs to be at least {} KiB",
                    min_swapsize >> 10
                ),
            }
        }
    }

    impl uucore::error::UError for MkswapError {
        fn code(&self) -> i32 {
            1
        }

        fn usage(&self) -> bool {
            false
        }
    }

    impl std::error::Error for MkswapError {}

    #[derive(Clone, Copy)]
    enum Endian {
        Native,
        Little,
        Big,
    }

    impl Endian {
        // Converts a native-endian value to this endianness.
        fn convert(&self, value: u32) -> u32 {
            match self {
                Self::Native => value,
                Self::Little => value.to_le(),
                Self::Big => value.to_be(),
            }
        }
    }

    const BOOTBITS_SIZE: usize = 1024;
    const SWAP_SIGNATURE: &[u8] = b"SWAPSPACE2";
    const SWAP_SIGNATURE_SZ: usize = SWAP_SIGNATURE.len();
    const SWAP_LABEL_LENGTH: usize = 16;
    const SWAP_VERSION: u32 = 1;
    const MIN_SWAP_PAGES: u32 = 10;
    const SWAP_UUID_LENGTH: usize = 16;

    #[repr(C)]
    struct SwapHeader {
        bootbits: [c_char; BOOTBITS_SIZE],
        version: u32,
        last_page: u32,
        nr_badpages: u32,
        uuid: [c_uchar; SWAP_UUID_LENGTH],
        label: [c_uchar; SWAP_LABEL_LENGTH],
        padding: [u32; 117],
        badpages: [u32; 1],
    }

    impl SwapHeader {
        fn new() -> Self {
            Self {
                bootbits: [0; BOOTBITS_SIZE],
                version: SWAP_VERSION,
                last_page: 0,
                nr_badpages: 0,
                uuid: [0; SWAP_UUID_LENGTH],
                label: [0; SWAP_LABEL_LENGTH],
                padding: [0; 117],
                badpages: [0],
            }
        }

        fn label(mut self, swaplabel: &str) -> Result<Self, MkswapError> {
            if swaplabel.len() > SWAP_LABEL_LENGTH {
                return Err(MkswapError::TooLongLabel);
            }
            let label_bytes = swaplabel.as_bytes();
            let lblen = label_bytes.len().min(SWAP_LABEL_LENGTH);
            self.label[..lblen].copy_from_slice(&label_bytes[..lblen]);

            Ok(self)
        }

        fn uuid(mut self, uuid: Uuid) -> Self {
            self.uuid = *uuid.as_bytes();
            self
        }

        fn pages(mut self, pages: u32) -> Result<Self, MkswapError> {
            if pages < MIN_SWAP_PAGES {
                return Err(MkswapError::TooFewPages { pages });
            }
            self.last_page = pages - 1;
            Ok(self)
        }

        fn nr_badpages(mut self, badpages: &[u32], pagesize: usize) -> Result<Self, MkswapError> {
            // space between swap signature and start of badpages
            let max_badpages = ((pagesize - SWAP_SIGNATURE_SZ) - offset_of!(SwapHeader, badpages))
                / size_of::<u32>();

            if badpages.len() > max_badpages {
                return Err(MkswapError::MaxBadPagesExceeded { max_badpages });
            }

            self.nr_badpages = badpages.len() as u32;
            Ok(self)
        }

        // Sets the endianness of all relevant fields
        // (version, nr_badpages, last_page).
        // Should be used last, after the fields are set
        fn set_endian(mut self, endianness: Endian) -> Self {
            self.version = endianness.convert(self.version);
            self.last_page = endianness.convert(self.last_page);
            self.nr_badpages = endianness.convert(self.nr_badpages);
            self
        }

        // Writes header fields into a signature page i.e. a buffer of size 'pagesize'
        fn write_to<W: Write + Seek>(&self, mut writer: W, pagesize: usize) -> io::Result<()> {
            writer.write_all(&[0u8; BOOTBITS_SIZE])?;
            writer.write_all(&self.version.to_ne_bytes())?;
            writer.write_all(&self.last_page.to_ne_bytes())?;
            writer.write_all(&self.nr_badpages.to_ne_bytes())?;
            writer.write_all(&self.uuid)?;
            writer.write_all(&self.label)?;

            writer.seek(SeekFrom::Start((pagesize - SWAP_SIGNATURE_SZ) as u64))?;
            writer.write_all(SWAP_SIGNATURE)?;
            writer.flush()?;
            Ok(())
        }
    }

    fn getpagesize() -> Result<usize, io::Error> {
        // both variable names are defined in POSIX and should work, but try both just in case
        let mut sz = unsafe { sysconf(_SC_PAGESIZE) };

        if sz <= 0 {
            sz = unsafe { sysconf(_SC_PAGE_SIZE) };
        }

        if sz <= 0 {
            Err(io::Error::other(
                "Failed to determine page size, please check your system configuration",
            ))
        } else {
            TryInto::<usize>::try_into(sz as u64).map_err(|_| {
                io::Error::other(format!(
                    "Page size too large, max page size: {}",
                    usize::MAX
                ))
            })
        }
    }

    // Get the size of a block device
    // Finds the size using ioctl or, as a backup, reading from sysfs
    fn get_blockdev_size(fd: &File, devname: &str) -> io::Result<u64> {
        let mut sz: u64 = 0;
        let err = unsafe { ioctl(fd.as_raw_fd(), BLKGETSIZE64 as c_ulong, &mut sz) };

        if sz == 0 || err < 0 {
            let f_size = File::open(format!("/sys/class/block/{devname}/size"))?;

            let mut reader = BufReader::new(f_size);
            let mut line = String::new();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                return Err(io::Error::other(format!(
                    "empty size file for block device {devname}"
                )));
            }

            let sectors = line.trim().parse::<u64>().map_err(|e| {
                io::Error::other(format!(
                    "Invalid size value for block device {devname}: {e}"
                ))
            })?;

            // get size in bytes by multiplying value from /sys/class, which is in 512 byte sectors
            match sectors.checked_mul(512) {
                Some(sz) => Ok(sz),
                None => Err(io::Error::other("Unable to determine size of block device")),
            }
        } else {
            Ok(sz)
        }
    }

    // Open and prepare a swap device
    // if createflag is true, sets appropriate permissions and preallocates the file
    fn open_device(device_path: &Path, createflag: bool, filesize: u64) -> io::Result<File> {
        let file = match OpenOptions::new()
            .create(createflag)
            .write(true)
            .read(true)
            .truncate(false)
            .append(false)
            .open(device_path)
        {
            Ok(f) => f,
            Err(e) => {
                return Err(io::Error::other(format!(
                    "cannot open {}: {}",
                    device_path.to_string_lossy(),
                    e
                )));
            }
        };
        let fd = file.as_raw_fd();

        if createflag {
            file.set_permissions(Permissions::from_mode(0o600))?;

            // check for COW filesystems
            let stat_fs = fstatfs(file.as_fd())?;
            if stat_fs.filesystem_type() == BTRFS_SUPER_MAGIC {
                let mut flags: uucore::libc::c_int = 0;
                let err = unsafe { ioctl(fd, FS_IOC_GETFLAGS as c_ulong, &mut flags) };
                if err < 0 {
                    return Err(io::Error::last_os_error());
                }

                // set NOCOW to disable copy-on-write for proper swapping
                // without this flag, on COW filesystems, swapon syscall fails
                flags |= FS_NOCOW_FL as uucore::libc::c_int;

                let err = unsafe { ioctl(fd.as_raw_fd(), FS_IOC_SETFLAGS as c_ulong, &mut flags) };
                if err < 0 {
                    return Err(io::Error::last_os_error());
                }
            }

            // fallocate to avoid holes in the created file
            if let Err(e) = fallocate(file.as_fd(), FallocateFlags::empty(), 0, filesize as i64) {
                return Err(io::Error::other(format!(
                    "{}: {}: Fallocate failed: {}",
                    uucore::util_name(),
                    device_path.to_string_lossy(),
                    e.desc()
                )));
            }
        }

        Ok(file)
    }

    // Checks block device for holes or pages that it can't read
    fn check_device(
        fd: &File,
        pagesize: usize,
        pages: u32,
        offset: u64,
        verbose: bool,
    ) -> Result<Vec<u32>, io::Error> {
        let mut buf = vec![0u8; pagesize];
        let mut badpages: Vec<u32> = Vec::new();

        for page in 1..pages {
            let pos = offset + (u64::from(page) * pagesize as u64);
            if fd.read_exact_at(&mut buf, pos).is_err() {
                badpages.push(page);
                if verbose {
                    eprintln!("bad page at index {page}");
                }
            }
        }
        Ok(badpages)
    }

    pub fn mkswap(matches: &ArgMatches) -> UResult<()> {
        let verboseflag = matches.get_flag("verbose");
        let checkflag = matches.get_flag("check");
        let forceflag = matches.get_flag("force");
        let offset = *matches.get_one::<u64>("offset").unwrap_or(&0u64);

        let Some(device) = matches.get_one::<String>("device") else {
            return Err(UUsageError::new(
                1,
                format!(
                    "error: Nowhere to set up swap on?\nTry '{} --help' for more information.",
                    uucore::util_name()
                ),
            ));
        };
        let devpath = Path::new(device.as_str());
        let devname = devpath
            .file_name()
            .and_then(|os| os.to_str())
            .unwrap_or_else(|| device.strip_prefix("/dev/").unwrap_or(device));

        let label = matches
            .get_one::<String>("label")
            .map_or("", String::as_str);
        if label.len() > SWAP_LABEL_LENGTH {
            return Err(MkswapError::TooLongLabel.into());
        }

        let endianness = match matches.get_one::<String>("endianness") {
            Some(str) => match str.to_lowercase().as_str() {
                "native" => Endian::Native,
                "little" => Endian::Little,
                "big" => Endian::Big,
                _ => {
                    return Err(UUsageError::new(
                        1,
                        format!("invalid endianness {} is not supported", str),
                    ));
                }
            },
            None => Endian::Native,
        };

        let uuid = match matches.get_one::<String>("uuid") {
            Some(str) => Uuid::from_str(str)
                .map_err(|e| USimpleError::new(1, format!("Invalid UUID '{str}': {e}")))?,
            None => Uuid::new_v4(),
        };

        let pagesize = {
            let sys_pagesize: usize = getpagesize()?;

            match matches.get_one::<usize>("pagesize") {
                Some(sz) => {
                    if !forceflag
                        && (*sz <= size_of::<SwapHeader>() + SWAP_SIGNATURE_SZ
                            || !sz.is_power_of_two())
                    {
                        return Err(USimpleError::new(
                            1,
                            format!("Bad user-specified page size {}", *sz),
                        ));
                    }

                    if *sz != sys_pagesize {
                        eprintln!(
                            "Using user-specified page size {}, instead of the system value {}",
                            *sz, sys_pagesize
                        );
                    }
                    *sz
                }
                None => sys_pagesize,
            }
        };

        let min_swapsize = (MIN_SWAP_PAGES as u64).saturating_mul(pagesize as u64);

        let createflag = matches.get_flag("file");
        let filesize = *matches.get_one::<u64>("filesize").unwrap_or(&0);
        if createflag && filesize < min_swapsize {
            return Err(MkswapError::SwapAreaTooSmall { min_swapsize }.into());
        }

        let mut fd = open_device(devpath, createflag, filesize)?;

        let stat = fd.metadata()?;
        if stat.st_uid() != 0 && unsafe { geteuid() } == 0 {
            eprintln!(
                "{}: {}: insecure file owner {}, fix with: chown 0:0 {}",
                uucore::util_name(),
                devname,
                stat.st_uid(),
                devpath.display()
            );
        }

        let devsize = if createflag {
            filesize
        } else if stat.file_type().is_block_device() {
            get_blockdev_size(&fd, devname)?
        } else {
            stat.st_size()
        };

        let swapsize = devsize.saturating_sub(offset);
        if swapsize < min_swapsize {
            return Err(MkswapError::SwapAreaTooSmall { min_swapsize }.into());
        }

        let pages: u32 = match ((devsize - offset) / pagesize as u64).try_into() {
            Ok(p) => p,
            Err(_) => {
                return Err(USimpleError::new(
                    1,
                    format!(
                        "error: swap area is too large: max size is {} GiB",
                        (u32::MAX as usize * pagesize) >> 30
                    ),
                ))
            }
        };

        let badpages = if checkflag {
            check_device(&fd, pagesize, pages, offset, verboseflag)?
        } else {
            Vec::new()
        };

        let hdr = SwapHeader::new()
            .label(label)?
            .pages(pages)?
            .uuid(uuid)
            .nr_badpages(&badpages, pagesize)?
            .set_endian(endianness);

        let mut sigpage = Cursor::new(vec![0u8; pagesize]);
        hdr.write_to(&mut sigpage, pagesize)?;

        if checkflag && !badpages.is_empty() {
            sigpage.seek(SeekFrom::Start(offset_of!(SwapHeader, badpages) as u64))?;
            for &page in &badpages {
                sigpage.write_all(&endianness.convert(page).to_ne_bytes())?;
            }
        }

        let sigpage = sigpage.into_inner();

        // Skip past bootbits to avoid overwriting data

        fd.seek(SeekFrom::Start(offset + BOOTBITS_SIZE as u64))?;
        fd.write_all(&sigpage[BOOTBITS_SIZE..])?;

        fd.flush()?;
        fd.sync_all()?;

        println!(
            "Setting up swapspace version 1, size = {} KiB ({} bytes)\n{}{}, UUID={}",
            (pages - 1) as usize * (pagesize / 1024),
            (pages - 1) as usize * pagesize,
            if label.is_empty() {
                "no label"
            } else {
                "LABEL="
            },
            &label[..label.floor_char_boundary(SWAP_LABEL_LENGTH)],
            uuid
        );

        Ok(())
    }
}

#[cfg(target_os = "linux")]
#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    use linux::*;
    let matches: clap::ArgMatches = uu_app().try_get_matches_from(args)?;
    if let Err(e) = mkswap(&matches) {
        uucore::error::set_exit_code(e.code());
        uucore::show_error!("{}", e);
    };
    Ok(())
}

#[cfg(not(target_os = "linux"))]
#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let _matches: ArgMatches = uu_app().try_get_matches_from(args)?;
    Err(USimpleError::new(1, "`mkswap` is available only on Linux."))
}

pub fn uu_app() -> Command {
    Command::new(uucore::util_name())
        .version(crate_version!())
        .about(ABOUT)
        .override_usage(format_usage(USAGE))
        .infer_long_args(true)
        .arg(
            Arg::new("device")
                .action(ArgAction::Set)
                .help("block device or swap file"),
        )
        .arg(
            Arg::new("label")
                .short('L')
                .long("label")
                .action(ArgAction::Set)
                .help("set a label"),
        )
        .arg(
            Arg::new("uuid")
                .short('U')
                .long("uuid")
                .action(ArgAction::Set)
                .help("set the UUID to use"),
        )
        .arg(
            Arg::new("file")
                .short('F')
                .long("file")
                .action(ArgAction::SetTrue)
                .requires("filesize")
                .help("create a swap file"),
        )
        .arg(
            Arg::new("filesize")
                .short('s')
                .long("size")
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(u64))
                .value_name("SIZE")
                .requires("file")
                .help("specify the size of the swap file in bytes"),
        )
        .arg(
            Arg::new("verbose")
                .long("verbose")
                .action(ArgAction::SetTrue)
                .help("verbose output"),
        )
        .arg(
            Arg::new("check")
                .short('c')
                .long("check")
                .action(ArgAction::SetTrue)
                .help("check the swap device for bad pages"),
        )
        .arg(
            Arg::new("pagesize")
                .short('p')
                .long("pagesize")
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(usize))
                .help("specify page size in bytes"),
        )
        .arg(
            Arg::new("force")
                .short('f')
                .long("force")
                .action(ArgAction::SetTrue)
                .help("allow swap size area to be larger than device"),
        )
        .arg(
            Arg::new("endianness")
                .short('e')
                .long("endianness")
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(String))
                .help("specify the endianness to use (native, little, or big)"),
        )
        .arg(
            Arg::new("offset")
                .short('o')
                .long("offset")
                .action(ArgAction::Set)
                .value_parser(clap::value_parser!(u64))
                .help("specify the offset in the device"),
        )

    // TODO: lock
}
