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
    use core::ffi::{c_char, c_uchar, c_void};
    use linux_raw_sys::ioctl::BLKGETSIZE64;
    use std::{
        fmt::Debug,
        fs::{File, Metadata, OpenOptions, Permissions},
        io::{BufRead, BufReader, Write},
        mem::size_of,
        os::{
            fd::AsRawFd,
            linux::fs::MetadataExt,
            unix::fs::{FileTypeExt, PermissionsExt},
        },
        path::Path,
        str::FromStr,
    };
    use uucore::{
        error::UUsageError,
        libc::{geteuid, ioctl, pread, sysconf, _SC_PAGESIZE, _SC_PAGE_SIZE},
    };
    use uuid::Uuid;

    const SWAP_SIGNATURE: &[u8] = "SWAPSPACE2".as_bytes();
    const SWAP_SIGNATURE_SZ: usize = SWAP_SIGNATURE.len();
    const SWAP_LABEL_LENGTH: usize = 16;
    const SWAP_VERSION: u32 = 1;
    const MIN_SWAP_PAGES: u32 = 10;
    const SWAP_UUID_LENGTH: usize = 16;

    #[derive(Debug)]
    pub enum MkswapError {
        TooLongLabel,
        TooFewPages { pages: u32 },
        MaxBadPagesExceeded { max_badpages: usize },
    }

    impl std::fmt::Display for MkswapError {
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
            }
        }
    }

    impl std::error::Error for MkswapError {}

    #[repr(C)]
    pub struct SwapHeader {
        bootbits: [c_char; 1024],
        version: u32,
        last_page: u32,
        nr_badpages: u32,
        uuid: [c_uchar; SWAP_UUID_LENGTH],
        label: [c_uchar; SWAP_LABEL_LENGTH],
        padding: [u32; 117],
        badpages: [u32; 1],
    }

    impl SwapHeader {
        pub fn new() -> Self {
            Self {
                bootbits: [0; 1024],
                version: SWAP_VERSION,
                last_page: 0,
                nr_badpages: 0,
                uuid: [0; SWAP_UUID_LENGTH],
                label: [0; SWAP_LABEL_LENGTH],
                padding: [0; 117],
                badpages: [0],
            }
        }

        pub fn label(mut self, swaplabel: String) -> Result<Self, MkswapError> {
            if swaplabel.len() > SWAP_LABEL_LENGTH {
                return Err(MkswapError::TooLongLabel);
            }
            let label_bytes = swaplabel.as_bytes();
            let lblen = label_bytes.len().min(SWAP_LABEL_LENGTH);
            self.label[..lblen].copy_from_slice(&label_bytes[..lblen]);

            Ok(self)
        }

        pub fn uuid(mut self, uuid: Uuid) -> Self {
            self.uuid = *uuid.as_bytes();
            self
        }

        pub fn last_page(mut self, pages: u32) -> Result<Self, MkswapError> {
            if pages < MIN_SWAP_PAGES {
                return Err(MkswapError::TooFewPages { pages });
            }
            self.last_page = match pages.checked_sub(1) {
                Some(page) => page,
                None => return Err(MkswapError::TooFewPages { pages: 0 }),
            };
            Ok(self)
        }

        /// calculates maximum amount of badpages that can fit in the signature page
        /// returns the space between start of badpages and swap signature in usize
        pub fn max_badpages(pagesize: usize) -> usize {
            let pagesize_bytes = pagesize * size_of::<usize>();

            (pagesize_bytes
                - 1024 * size_of::<u8>() // bootbits
                - 120 * size_of::<i32>() // padding + nr_badpages + version
		- SWAP_LABEL_LENGTH * size_of::<u8>()
		- SWAP_UUID_LENGTH * size_of::<u8>()
		- SWAP_SIGNATURE_SZ * size_of::<u8>())
                / size_of::<usize>()
        }

        pub fn bad_pages(mut self, badpages: &[u32], pagesize: usize) -> Result<Self, MkswapError> {
            self.nr_badpages = badpages.len() as u32;
            let max_badpages = SwapHeader::max_badpages(pagesize);

            if self.nr_badpages as usize > max_badpages {
                return Err(MkswapError::MaxBadPagesExceeded { max_badpages });
            }

            Ok(self)
        }
    }

    /// Retrieves system page size with ioctl
    fn getpagesize() -> Result<usize, std::io::Error> {
        // both variable names are defined in POSIX and should work, but try both just in case
        let mut sz = unsafe { sysconf(_SC_PAGESIZE) };
        if sz <= 1 {
            sz = unsafe { sysconf(_SC_PAGE_SIZE) };
        }

        if sz <= 0 {
            Err(std::io::Error::other(
                "Failed to determine page size, please check your system configuration",
            ))
        } else {
            TryInto::<usize>::try_into(sz as u64).map_err(|_| {
                std::io::Error::other(format!(
                    "Page size too large, max page size: {}",
                    usize::MAX
                ))
            })
        }
    }

    /// Get the size of a file or block device from a file descriptor
    fn getsize(fd: &File, stat: &Metadata, devname: &str) -> Result<u64, std::io::Error> {
        if !stat.file_type().is_block_device() {
            return Ok(stat.st_size());
        }

        let mut sz: u64 = 0;
        let err = unsafe { ioctl(fd.as_raw_fd(), BLKGETSIZE64 as u64, &mut sz) };

        if sz == 0 || err < 0 {
            let f_size = File::open(format!("/sys/class/block/{devname}/size"))?;

            let mut reader = BufReader::new(f_size);
            let mut line = String::new();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                return Err(std::io::Error::other(format!(
                    "empty size file for block device {devname}"
                )));
            }

            let sectors = line.trim().parse::<u64>().map_err(|e| {
                std::io::Error::other(format!(
                    "Invalid size value for block device {devname}: {e}"
                ))
            })?;

            // get size in bytes by multiplying value from /sys/class, which is in 512 byte sectors
            match sectors.checked_mul(512) {
                Some(sz) => Ok(sz),
                None => Err(std::io::Error::other(
                    "Unable to determine size of block device",
                )),
            }
        } else {
            Ok(sz)
        }
    }

    /// Check device for holes
    fn check_device(
        fd: &File,
        pagesize: usize,
        pages: u32,
        verbose: bool,
    ) -> Result<Vec<u32>, std::io::Error> {
        let mut buf = vec![0u8; pagesize];
        let mut badpages: Vec<u32> = Vec::new();

        for page in 1..pages {
            let bytes = unsafe {
                pread(
                    fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut c_void,
                    pagesize,
                    page as i64 * pagesize as i64,
                )
            };
            if bytes < pagesize as isize {
                badpages.push(page);
                if verbose {
                    eprintln!("bad page at index {page}");
                }
            }
        }
        Ok(badpages)
    }

    fn open_device(
        device: &String,
        dev: &Path,
        createflag: bool,
        filesize: u64,
    ) -> Result<File, std::io::Error> {
        let fd = match OpenOptions::new()
            .create(createflag)
            .write(true)
            .read(true)
            .truncate(false)
            .append(false)
            .open(dev)
        {
            Ok(f) => f,
            Err(e) => {
                return Err(std::io::Error::other(format!(
                    "failed to open {device}: {e}",
                )));
            }
        };

        if createflag {
            // TODO: check for existing filesystem signatures and set NOCOW flag for swapfiles to work on btrfs
            fd.set_permissions(Permissions::from_mode(0o600))?;
            fd.set_len(filesize)?;
        }

        Ok(fd)
    }

    fn init_signature_page(
        pagesize: usize,
        pages: u32,
        uuid: Uuid,
        label: &str,
        badpages: Vec<u32>,
    ) -> Result<Vec<u8>, MkswapError> {
        let mut buf = vec![0u8; pagesize];

        let header = SwapHeader::new()
            .label(label.to_owned())?
            .last_page(pages)?
            .bad_pages(&badpages, pagesize)?
            .uuid(uuid);

        let header_bytes = unsafe {
            std::slice::from_raw_parts(
                (&header as *const SwapHeader) as *const u8,
                size_of::<SwapHeader>(),
            )
        };

        buf[..header_bytes.len()].copy_from_slice(header_bytes);

        if !badpages.is_empty() {
            let badpages_bytes = unsafe {
                std::slice::from_raw_parts(
                    badpages.as_ptr() as *const u8,
                    badpages.len() * size_of::<u32>(),
                )
            };
            let badpages_offset = pagesize
                - (SwapHeader::max_badpages(pagesize) * size_of::<u32>())
                - SWAP_SIGNATURE_SZ;
            let badpages_n_bytes = badpages.len() * size_of::<u32>();
            buf[badpages_offset..badpages_offset + badpages_n_bytes]
                .copy_from_slice(badpages_bytes);
        }
        buf[pagesize - SWAP_SIGNATURE_SZ..].copy_from_slice(SWAP_SIGNATURE);
        Ok(buf)
    }

    pub fn mkswap(matches: &ArgMatches) -> UResult<()> {
        let verbose = matches.get_flag("verbose");
        let checkflag = matches.get_flag("check");
        let createflag = matches.get_flag("file");
        let forceflag = matches.get_flag("force");

        let device = match matches.get_one::<String>("device") {
            Some(str) => str,
            None => {
                return Err(UUsageError::new(
                    1,
                    format!(
                        "error: Nowhere to set up swap on?\n Try '{} --help' for more information.",
                        uucore::util_name()
                    ),
                ));
            }
        };

        let pagesize_arg = matches
            .try_get_one::<usize>("pagesize")
            .map_err(|e| USimpleError {
                code: 1,
                message: e.to_string(),
            })?;
        let pagesize = {
            let sys_pagesize: usize = getpagesize()?;

            match pagesize_arg {
                Some(sz) => {
                    if !forceflag && (*sz <= size_of::<SwapHeader>() || !sz.is_power_of_two()) {
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

        let label = match matches.get_one::<String>("label") {
            Some(l) => l.as_str(),
            None => "",
        };

        let dev = Path::new(device.as_str());
        let devname = {
            if let Some(n) = dev.file_name().and_then(|o| o.to_str()) {
                n
            } else {
                device.strip_prefix("/dev/").unwrap_or(device)
            }
        };

        let filesize = *matches.get_one::<u64>("filesize").unwrap_or(&0);
        let mut fd = open_device(device, dev, createflag, filesize)?;
        let stat = fd.metadata()?;
        if stat.st_uid() != 0 && unsafe { geteuid() } == 0 {
            eprintln!(
                "mkswap: {}: insecure file owner {}, fix with: chown 0:0 {}",
                device,
                stat.st_uid(),
                device
            );
        }

        let uuid = match matches.get_one::<String>("uuid") {
            Some(str) => Uuid::from_str(str)
                .map_err(|e| USimpleError::new(1, format!("Invalid UUID '{str}': {e}")))?,
            None => Uuid::new_v4(),
        };

        let devsize = if createflag {
            filesize
        } else {
            getsize(&fd, &stat, devname).map_err(|e| {
                USimpleError::new(
                    e.raw_os_error().unwrap_or(1),
                    "Unable to determine size of swap device",
                )
            })?
        };

        let min_swapsize_kib = (MIN_SWAP_PAGES * pagesize as u32) / 1024;
        if devsize / 1024 < min_swapsize_kib as u64 {
            return Err(USimpleError::new(
                1,
                format!(
                    "error: swap area needs to be at least {} KiB",
                    min_swapsize_kib
                ),
            ));
        }

        let pages: u32 = match (devsize / pagesize as u64).try_into() {
            Ok(p) => p,
            Err(_) => {
                return Err(USimpleError::new(
                    1,
                    format!("{} is too large: max size is {} bytes", devname, u32::MAX),
                ))
            }
        };

        if pages < MIN_SWAP_PAGES {
            return Err(USimpleError::new(
                1,
                format!(
                    "Device {} is too small for a swap area, minimum size is {}KiB",
                    devname, min_swapsize_kib
                ),
            ));
        }

        let badpages = if checkflag {
            check_device(&fd, pagesize, pages, verbose)?
        } else {
            vec![]
        };

        let buf = match init_signature_page(pagesize, pages, uuid, label, badpages) {
            Ok(buffer) => buffer,
            Err(MkswapError::TooFewPages { pages: _ }) => {
                return Err(USimpleError::new(
                    1,
                    format!(
                        "Device {} is too small for a swap area, minimum size is {}KiB",
                        devname,
                        (MIN_SWAP_PAGES * pagesize as u32) / 1024
                    ),
                ));
            }
            Err(MkswapError::TooLongLabel) => {
                return Err(USimpleError::new(
                    1,
                    format!("{}", MkswapError::TooLongLabel),
                ));
            }
            Err(MkswapError::MaxBadPagesExceeded { max_badpages: e }) => {
                return Err(USimpleError::new(
                    1,
                    format!("{}", MkswapError::MaxBadPagesExceeded { max_badpages: (e) }),
                ));
            }
        };

        fd.write_all(&buf)?;
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
                .short('u')
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
                .short('v')
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

    // TODO: endianness, offset, lock
}
