// This file is part of the uutils util-linux package.
//
// For the full copyright and license information, please view the LICENSE
// file that was distributed with this source code.

#[cfg(target_os = "linux")]
mod linux {
    use uutests::{at_and_ucmd, new_ucmd};

    #[test]
    fn test_invalid_path() {
        new_ucmd!()
            .arg("/foo/bar/baz")
            .fails()
            .code_is(1)
            .stderr_contains("failed to open /foo/bar/baz: No such file or directory");
    }

    #[test]
    fn test_directory_err() {
        let (at, mut ucmd) = at_and_ucmd!();
        at.mkdir("foo");
        ucmd.arg("foo")
            .fails()
            .code_is(1)
            .stderr_contains("failed to open foo: Is a directory");
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
    fn test_swapfile() {
        let (at, mut ucmd) = at_and_ucmd!();
        at.write_bytes("swapfile", &[0; 65536]);
        ucmd.arg("swapfile")
            .succeeds()
            .code_is(0)
            .stdout_contains("Setting up swapspace version 1");
    }

    #[test]
    fn test_swaplabel() {
        let (at, mut ucmd) = at_and_ucmd!();
        at.write_bytes("swap", &[0; 65536]);
        ucmd.arg("swap")
            .arg("-L")
            .arg("SWAPLABEL")
            .succeeds()
            .code_is(0)
            .stdout_contains("LABEL=SWAPLABEL,")
            .stdout_contains("Setting up swapspace version 1");
    }

    #[test]
    fn test_custom_uuid() {
        let (at, mut ucmd) = at_and_ucmd!();
        at.write_bytes("swap", &[0; 65536]);
        ucmd.arg("swap")
            .arg("-L")
            .arg("SWAP")
            .arg("-u")
            .arg("4adbb628-19fa-4bef-9c60-8ce030381672")
            .succeeds()
            .code_is(0)
            .stdout_contains("LABEL=SWAP, UUID=4adbb628-19fa-4bef-9c60-8ce030381672")
            .stdout_contains("Setting up swapspace version 1");
    }

    #[test]
    fn test_long_label() {
        let (at, mut ucmd) = at_and_ucmd!();
        at.write_bytes("swap", &[0; 65536]);
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
        at.write_bytes("swap", &[0; 65536]);
        ucmd.arg("swap")
            .arg("-L")
            .arg("SWAP")
            .arg("-u")
            .arg("078d9a95+4c1e-4961-b8a5-3f9d27586645")
            .fails()
            .code_is(1)
            .stderr_contains("Invalid UUID '078d9a95+4c1e-4961-b8a5-3f9d27586645':");
    }

    #[test]
    fn test_create_file() {
        use std::io::Read;
        let (at, mut ucmd) = at_and_ucmd!();
        ucmd.arg("swapfile")
            .arg("-F")
            .arg("-s")
            .arg("65535")
            .succeeds()
            .code_is(0)
            .stdout_contains("Setting up swapspace version 1");
        at.file_exists("swapfile");

        let mut buf = vec![0u8; 4096];

        let mut fd = at.open("swapfile");
        fd.read_exact(&mut buf).unwrap();

        let sig = &buf[4086..];
        let swapsig = "SWAPSPACE2".as_bytes();
        assert_eq!(sig, swapsig);
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
            .arg("65535")
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
            .arg("65535")
            .arg("-p")
            .arg("512")
            .fails()
            .code_is(1)
            .stderr_contains("Bad user-specified page size 512");
        new_ucmd!()
            .arg("-F")
            .arg("-s")
            .arg("65535")
            .arg("-p=-1")
            .fails()
            .code_is(1)
            .stderr_contains(
                "invalid value '-1' for '--pagesize <pagesize>': invalid digit found in string",
            );
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
