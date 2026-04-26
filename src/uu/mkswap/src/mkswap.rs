// This file is part of the uutils util-linux package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

use clap::{crate_version, Arg, ArgAction, ArgMatches, Command};
use uucore::error::{set_exit_code, UResult, USimpleError, UUsageError};
use uucore::{format_usage, help_about, help_usage};

const ABOUT: &str = help_about!("mkswap.md");
const USAGE: &str = help_usage!("mkswap.md");

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::{
        fmt::Debug,
        fs::{self, File, Metadata},
        io::{BufRead, BufReader, Write},
        mem::size_of,
        os::{
            fd::AsRawFd,
            linux::fs::MetadataExt,
            raw::{c_char, c_uchar, c_void},
            unix::fs::{FileTypeExt, PermissionsExt},
        },
        path::Path,
        str::FromStr,
    };

    use linux_raw_sys::ioctl::BLKGETSIZE64;
    use uucore::libc::{ioctl, pread, sysconf, _SC_PAGESIZE, _SC_PAGE_SIZE};
    use uuid::Uuid;

    pub const SWAP_SIGNATURE: &[u8] = "SWAPSPACE2".as_bytes();
    pub const SWAP_SIGNATURE_SZ: usize = SWAP_SIGNATURE.len();
    pub const SWAP_LABEL_LENGTH: usize = 16;
    pub const SWAP_VERSION: u32 = 1;
    pub const MIN_SWAP_PAGES: u32 = 10;

    #[derive(Debug, Clone)]
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
        uuid: [c_uchar; 16],
        label: [c_uchar; SWAP_LABEL_LENGTH],
        padding: [u32; 117],
        badpages: [u32; 1],
    }

    impl SwapHeader {
        pub fn new() -> Self {
            Self {
                bootbits: [0i8; 1024],
                version: SWAP_VERSION,
                last_page: 0,
                nr_badpages: 0,
                uuid: [0u8; 16],
                label: [0u8; SWAP_LABEL_LENGTH],
                padding: [0u32; 117],
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

        pub fn pages(mut self, pages: u32) -> Result<Self, MkswapError> {
            if pages < MIN_SWAP_PAGES {
                return Err(MkswapError::TooFewPages { pages });
            }
            self.last_page = pages - 1;
            Ok(self)
        }

        pub fn bad_pages(
            mut self,
            badpages: Vec<u32>,
            pagesize: usize,
        ) -> Result<Self, MkswapError> {
            self.nr_badpages = (badpages.len() as u32).saturating_sub(1);
            let max_badpages = (pagesize
                - 1024 * size_of::<u8>()
                - 120 * size_of::<i32>()
                - 32 * size_of::<u8>()
                - SWAP_SIGNATURE_SZ)
                / size_of::<i32>();

            if self.nr_badpages > max_badpages as u32 {
                return Err(MkswapError::MaxBadPagesExceeded { max_badpages });
            }
            Ok(self)
        }
    }

    fn getpagesize() -> Result<usize, std::io::Error> {
        let mut sz = unsafe { sysconf(_SC_PAGESIZE) };
        if sz < 512 {
            sz = unsafe { sysconf(_SC_PAGE_SIZE) };
        }
        if sz <= 0 {
            Err(std::io::Error::other(
                "Failed to determine page size, please check your system configuration",
            ))
        } else {
            Ok(sz as usize)
        }
    }

    fn getsize(fd: &File, stat: &Metadata, devname: &str) -> Result<usize, std::io::Error> {
        match stat.file_type().is_block_device() {
            true => {
                let mut sz: usize = 0;
                let err = unsafe { ioctl(fd.as_raw_fd(), BLKGETSIZE64 as u64, &mut sz) };

                if sz == 0 || err < 0 {
                    let f_size = fs::File::open(format!("/sys/class/block/{devname}/size"))?;

                    let mut reader = BufReader::new(f_size);
                    let mut line = String::new();
                    let bytes = reader.read_line(&mut line)?;
                    if bytes == 0 {
                        return Err(std::io::Error::other(format!(
                            "empty size file for block device {devname}"
                        )));
                    }

                    let sectors = line.trim().parse::<usize>().map_err(|e| {
                        std::io::Error::other(format!(
                            "Invalid size value for block device {devname}: {e}"
                        ))
                    })?;

                    match sectors.checked_mul(512) {
                        Some(sz) => Ok(sz),
                        None => Err(std::io::Error::other(
                            "Integer overflow while trying to determine size of block device",
                        )),
                    }
                } else {
                    Ok(sz)
                }
            }
            false => Ok(stat.st_size() as usize),
        }
    }

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
        let mut options = fs::OpenOptions::new();
        let fd = match options
            .create(false)
            .create_new(createflag)
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
            fd.set_permissions(fs::Permissions::from_mode(0o600))?;
            fd.set_len(filesize)?;
        }

        Ok(fd)
    }

    fn write_signature_page(
        pagesize: usize,
        pages: u32,
        uuid: Uuid,
        label: &str,
        badpages: Vec<u32>,
    ) -> Result<Vec<u8>, MkswapError> {
        let mut buf = vec![0u8; pagesize];

        let header = SwapHeader::new()
            .label(label.to_owned())?
            .pages(pages)?
            .bad_pages(badpages, pagesize)?
            .uuid(uuid);

        let header_bytes = unsafe {
            std::slice::from_raw_parts(
                (&header as *const SwapHeader) as *const u8,
                size_of::<SwapHeader>(),
            )
        };

        buf[..header_bytes.len()].copy_from_slice(header_bytes);
        buf[pagesize - SWAP_SIGNATURE_SZ..].copy_from_slice(SWAP_SIGNATURE);
        Ok(buf)
    }

    pub fn mkswap(matches: &ArgMatches) -> UResult<()> {
        let verbose = matches.get_flag("verbose");
        let checkflag = matches.get_flag("check");
        let createflag = matches.get_flag("file");
        let pagesize_arg = matches
            .try_get_one::<u64>("pagesize")
            .map_err(|e| USimpleError {
                code: 1,
                message: e.to_string(),
            })?;
        let filesize = *matches.get_one::<u64>("filesize").unwrap_or(&0);

        let device = match matches.get_one::<String>("device") {
            Some(str) => str,
            None => {
                return Err(UUsageError::new(
                    1,
                    format!(
                        "Usage: {}\nFor more information, try '--help'.",
                        format_usage(USAGE)
                    ),
                ))
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
        let mut fd = open_device(device, dev, createflag, filesize)?;

        let stat = fd.metadata()?;
        if stat.st_uid() != 0 {
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

        let pagesize = match pagesize_arg {
            Some(sz) => *sz as usize, // TODO: check if its power of 2
            None => getpagesize()?,
        };
        let devsize = if createflag {
            filesize as usize
        } else {
            getsize(&fd, &stat, devname).map_err(|e| {
                USimpleError::new(
                    e.raw_os_error().unwrap_or(1),
                    "Unable to determine size of swap device",
                )
            })?
        };

        let pages = if devsize > 0 {
            (devsize / pagesize) as u32 - 1
        } else {
            0
        };

        let badpages = if checkflag {
            check_device(&fd, pagesize, pages, verbose)?
        } else {
            vec![0]
        };

        let buf = match write_signature_page(pagesize, pages, uuid, label, badpages) {
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
            "Setting up swapspace version 1, size = {}KiB\n{}{}, UUID={}",
            (((pages - 1) as usize * pagesize) / 1024),
            if label.is_empty() {
                "No label"
            } else {
                "LABEL="
            },
            &label[..label.len().min(16)],
            uuid
        );

        Ok(())
    }
}

#[cfg(target_os = "linux")]
use linux::*;

#[cfg(target_os = "linux")]
#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let matches: clap::ArgMatches = uu_app().try_get_matches_from(args)?;
    if let Err(e) = mkswap(&matches) {
        set_exit_code(2);
        uucore::show_error!("{}", e);
    };
    Ok(())
}

#[cfg(not(target_os = "linux"))]
#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let _matches: clap::ArgMatches = uu_app().try_get_matches_from(args)?;

    Err(uucore::error::USimpleError::new(
        1,
        "`mkswap` is available only on Linux.",
    ))
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
                .help("size of the swap file in bytes"),
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
                .value_parser(clap::value_parser!(u64))
                .help("specify page size in bytes"),
        )
    // TODO: check, endianness, offset, force
}
