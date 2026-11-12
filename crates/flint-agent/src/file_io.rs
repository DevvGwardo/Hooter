//! Descriptor-checked regular-file reads for saved history and file evidence.

pub(crate) fn open_regular_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // A FIFO can block in open(), before descriptor metadata is available.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Not a regular file.",
        ));
    }
    Ok(file)
}

pub(crate) fn file_link_count(file: &std::fs::File) -> std::io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Ok(file.metadata()?.nlink())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, GetFileInformationByHandle,
        };

        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if (info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Reparse points are not regular files.",
            ));
        }
        Ok(u64::from(info.nNumberOfLinks))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Ok(1)
    }
}
