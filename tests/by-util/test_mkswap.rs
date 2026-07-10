// This file is part of the uutils util-linux package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

#[cfg(target_os = "linux")]
mod linux {
    use uutests::{at_and_ucmd, new_ucmd};

    const SWAP_SIGNATURE: &[u8] = "SWAPSPACE2".as_bytes();

    const SWAP_VERSION: u32 = 1;
    const SWAP_VERSION_OFFSET: usize = 1024;

    const SWAP_UUID_LENGTH: usize = 16;
    const SWAP_UUID_OFFSET: usize = 1036;

    #[test]
    fn test_invalid_path() {
        new_ucmd!()
            .arg("foobar/barfoo/bazoo")
            .fails()
            .code_is(1)
            .stderr_contains("cannot open foobar/barfoo/bazoo: No such file or directory");
    }

    #[test]
    fn test_directory_err() {
        let (at, mut ucmd) = at_and_ucmd!();
        at.mkdir("foo");
        ucmd.arg("foo")
            .fails()
            .code_is(1)
            .stderr_contains("cannot open foo: Is a directory");
    }

    #[test]
    fn test_invalid_arg() {
        new_ucmd!().arg("foo").fails().code_is(1);
    }

    #[test]
    fn test_empty_args() {
        new_ucmd!()
            .fails()
            .code_is(1)
            .stderr_contains("Nowhere to set up swap on?");
    }

    #[test]
    fn test_empty_file() {
        let (at, mut ucmd) = at_and_ucmd!();
        at.touch("empty");
        ucmd.arg("empty")
            .fails()
            .stderr_contains("swap area needs to be at least");
    }

    #[test]
    fn test_min_size() {
        let (at, mut ucmd) = at_and_ucmd!();
        at.write_bytes("swap", &[0; 4096]);
        ucmd.arg("swap")
            .fails()
            .stderr_contains("swap area needs to be at least");
    }

    #[test]
    fn test_existing_file() {
        let (at, mut ucmd) = at_and_ucmd!();
        at.write_bytes("swapfile", &[0; 40960]);
        ucmd.arg("swapfile")
            .succeeds()
            .code_is(0)
            .stdout_contains("Setting up swapspace version 1");
        let buf = at.read_bytes("swapfile");

        let buf_version = u32::from_ne_bytes(buf[1024..1028].try_into().unwrap());
        assert_eq!(SWAP_VERSION, buf_version);
    }

    #[test]
    fn test_swaplabel() {
        let (at, mut ucmd) = at_and_ucmd!();
        at.write_bytes("swap_label_test", &[0; 40960]);
        ucmd.arg("swap_label_test")
            .arg("-L")
            .arg("SWAPLABEL")
            .succeeds()
            .code_is(0)
            .stdout_contains("LABEL=SWAPLABEL")
            .stdout_contains("Setting up swapspace version 1");

        let buf = at.read_bytes("swap_label_test");
        assert_eq!(&buf[1052..1061], b"SWAPLABEL")
    }

    #[test]
    fn test_custom_uuid() {
        let (at, mut ucmd) = at_and_ucmd!();
        at.write_bytes("swap_uuid_test", &[0; 40960]);
        ucmd.arg("swap_uuid_test")
            .arg("-L")
            .arg("SWAP")
            .arg("-U")
            .arg("4adbb628-19fa-4bef-9c60-8ce030381672")
            .succeeds()
            .code_is(0)
            .stdout_contains("LABEL=SWAP, UUID=4adbb628-19fa-4bef-9c60-8ce030381672")
            .stdout_contains("Setting up swapspace version 1");

        let buf = at.read_bytes("swap_uuid_test");
        assert_eq!(
            &buf[SWAP_UUID_OFFSET..SWAP_UUID_OFFSET + SWAP_UUID_LENGTH],
            [74, 219, 182, 40, 25, 250, 75, 239, 156, 96, 140, 224, 48, 56, 22, 114]
        );
    }

    #[test]
    fn test_long_label() {
        let (at, mut ucmd) = at_and_ucmd!();
        at.write_bytes("mkswap_test_long_label", &[0; 40960]);
        ucmd.arg("swap")
            .arg("-L")
            .arg("OUTRAGEOUSLYLONGSWAPLABEL")
            .fails()
            .code_is(1)
            .stderr_contains("Label is too long, maximum size is 16 characters");
    }

    #[test]
    fn test_invalid_uuid() {
        let (at, mut ucmd) = at_and_ucmd!();
        at.write_bytes("mkswap_invalid_uuid", &[0; 40960]);
        ucmd.arg("mkswap_invalid_uuid")
            .arg("-L")
            .arg("SWAP")
            .arg("-U")
            .arg("078d9a95+4c1e-4961-b8a5-3f9d27586645")
            .fails()
            .code_is(1)
            .stderr_contains("Invalid UUID '078d9a95+4c1e-4961-b8a5-3f9d27586645':");
    }

    #[test]
    fn test_create_file() {
        use std::io::Read;
        let (at, mut ucmd) = at_and_ucmd!();
        ucmd.arg("mkswap_create_file")
            .arg("-F")
            .arg("-s")
            .arg("40960")
            .succeeds()
            .code_is(0)
            .stdout_contains("Setting up swapspace version 1");
        assert!(at.file_exists("mkswap_create_file"));

        let mut buf = vec![0u8; 4096];
        {
            let mut fd = at.open("mkswap_create_file");
            fd.read_exact(&mut buf).unwrap();
        }

        let sig = &buf[4086..];
        assert_eq!(SWAP_SIGNATURE, sig);
    }

    #[test]
    fn test_negative_filesize() {
        new_ucmd!()
            .arg("-F")
            .arg("test_swapfile")
            .arg("-s=-1")
            .fails()
            .code_is(1)
            .stderr_contains("invalid value");
    }

    #[test]
    fn test_missing_required_args() {
        new_ucmd!()
            .arg("-F")
            .arg("swapfile")
            .fails()
            .code_is(1)
            .stderr_contains("the following required arguments were not provided:")
            .stderr_contains("--size");
    }

    #[test]
    fn test_bad_page_size() {
        new_ucmd!()
            .arg("-F")
            .arg("test_swapfile")
            .arg("-s")
            .arg("40960")
            .arg("-p")
            .arg("4000")
            .fails()
            .code_is(1)
            .stderr_contains("Bad user-specified page size 4000");
    }

    #[test]
    fn test_too_small_page_size() {
        new_ucmd!()
            .arg("-F")
            .arg("test_swapfile")
            .arg("-s")
            .arg("40960")
            .arg("-p")
            .arg("512")
            .fails()
            .code_is(1)
            .stderr_contains("Bad user-specified page size 512");
        new_ucmd!()
            .arg("-F")
            .arg("-s")
            .arg("40960")
            .arg("-p=-1")
            .fails()
            .code_is(1)
            .stderr_contains(
                "invalid value '-1' for '--pagesize <pagesize>': invalid digit found in string",
            );
    }

    #[test]
    fn test_endianness_big() {
        use std::io::Read;
        let (at, mut ucmd) = at_and_ucmd!();
        ucmd.arg("swapfile_endianness_test")
            .arg("--pagesize")
            .arg("4096")
            .arg("-F")
            .arg("-s")
            .arg("40960") // 10 pages
            .arg("--endianness")
            .arg("big")
            .succeeds()
            .code_is(0);
        assert!(at.file_exists("swapfile_endianness_test"));

        let mut buf = [0u8; 4096];
        {
            let mut fd = at.open("swapfile_endianness_test");
            fd.read_exact(&mut buf).unwrap();
        }

        let be_version = 1u32.to_be();
        let version = u32::from_ne_bytes(
            buf[SWAP_VERSION_OFFSET..SWAP_VERSION_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(be_version, version);

        const PAGES: u32 = 10;
        let be_last_page = (PAGES - 1).to_be();
        let last_page = u32::from_ne_bytes(buf[1028..1032].try_into().unwrap());
        assert_eq!(be_last_page, last_page);
    }

    #[test]
    fn test_offset() {
        use std::io::Read;
        let (at, mut ucmd) = at_and_ucmd!();
        ucmd.arg("mkswap_test_offset")
            .arg("--pagesize")
            .arg("4096")
            .arg("-F")
            .arg("-s")
            .arg("45056")
            .arg("--offset")
            .arg("4096")
            .succeeds()
            .code_is(0);

        let offset: usize = 4096;
        let mut buf = [0u8; 8192];

        let mut fd = at.open("mkswap_test_offset");
        fd.read_exact(&mut buf).unwrap();

        let sig = &buf[4086 + offset..];
        assert_eq!(SWAP_SIGNATURE, sig);

        let buf_version = u32::from_ne_bytes(buf[1024 + offset..1028 + offset].try_into().unwrap());
        assert_eq!(SWAP_VERSION, buf_version);
    }
}

#[cfg(not(target_os = "linux"))]
mod non_linux {
    use uutests::new_ucmd;

    #[test]
    fn test_fails_on_unsupported_platforms() {
        new_ucmd!()
            .fails()
            .code_is(1)
            .stderr_is("mkswap: `mkswap` is available only on Linux.\n");
    }
}
