//! Pinned non-symlink file reads shared by configured instructions and format planning.

use super::configured_skills::metadata_is_link_like;
use std::fs::File;
use std::io;
use std::path::Path;

/// Stable native object identity, retained only as a bounded digest in plans.
pub(crate) fn file_identity(file: &File) -> io::Result<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok([metadata.dev().to_be_bytes(), metadata.ino().to_be_bytes()].concat())
    }
    #[cfg(windows)]
    {
        let (volume, id) = windows_file_identity(file)?;
        Ok([volume.to_be_bytes().as_slice(), id.as_slice()].concat())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file identity unavailable",
        ))
    }
}

/// Return the number of hard links to the file referenced by the handle.
pub(crate) fn file_link_count(file: &File) -> io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok(metadata.nlink())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.nNumberOfLinks as u64)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file link count unavailable",
        ))
    }
}

pub(crate) fn directory_identity(path: &Path) -> io::Result<Vec<u8>> {
    #[cfg(unix)]
    {
        file_identity(&File::from(open_unix_directory(path)?))
    }
    #[cfg(windows)]
    {
        file_identity(&File::from(open_windows_directory(path)?))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory identity unavailable",
        ))
    }
}

#[cfg(windows)]
const WINDOWS_FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
#[cfg(windows)]
const WINDOWS_FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
#[cfg(windows)]
const WINDOWS_OBJ_CASE_INSENSITIVE: u32 = 0x0000_0040;
#[cfg(windows)]
const WINDOWS_OBJ_DONT_REPARSE: u32 = 0x0000_1000;

// File authority names ordinary files, not redirectable filesystem
// trees. Resolve the path through pinned directory handles so an ancestor
// cannot be swapped to a symlink/reparse point between validation and open.
pub(crate) fn open_regular_file(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        open_regular_file_unix(path, || {})
    }
    #[cfg(windows)]
    {
        open_regular_file_windows(path, || {})
    }
    #[cfg(not(any(unix, windows)))]
    {
        unsupported_regular_file_open(path)
    }
}

#[cfg(any(test, not(any(unix, windows))))]
pub(crate) fn unsupported_regular_file_open(_path: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure configured instruction reads are unsupported on this platform",
    ))
}

#[cfg(unix)]
pub(crate) fn open_regular_file_unix(path: &Path, before_leaf: impl FnOnce()) -> io::Result<File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};

    let parent_path = path
        .parent()
        .ok_or_else(|| io::Error::other("instruction path must name an absolute file"))?;
    let parent = open_unix_directory(parent_path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::other("instruction parent is unavailable")
        } else {
            error
        }
    })?;
    let leaf = path
        .file_name()
        .ok_or_else(|| io::Error::other("instruction path must name an absolute file"))?;
    let leaf = CString::new(std::os::unix::ffi::OsStrExt::as_bytes(leaf))
        .map_err(|_| io::Error::other("instruction path contains NUL"))?;

    before_leaf();

    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound
            && !unix_parent_still_current(parent_path, parent.as_raw_fd())
        {
            return Err(io::Error::other(
                "instruction parent changed during observation",
            ));
        }
        return Err(error);
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata_is_link_like(&metadata) {
        return Err(io::Error::other(
            "instruction handle is not an ordinary file",
        ));
    }
    if !unix_parent_still_current(parent_path, parent.as_raw_fd()) {
        return Err(io::Error::other(
            "instruction parent changed during observation",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_unix_directory(path: &Path) -> io::Result<std::os::fd::OwnedFd> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    if !path.is_absolute() {
        return Err(io::Error::other("instruction parent must be absolute"));
    }
    let root_fd = unsafe { libc::open(b"/\0".as_ptr().cast(), unix_directory_open_flags()) };
    if root_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut directory = unsafe { OwnedFd::from_raw_fd(root_fd) };
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(io::Error::other(
                    "instruction path must be absolute without parent traversal",
                ));
            }
        };
        let name = CString::new(name.as_bytes())
            .map_err(|_| io::Error::other("instruction path contains NUL"))?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                unix_directory_open_flags(),
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        directory = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    Ok(directory)
}

#[cfg(unix)]
fn unix_parent_still_current(path: &Path, expected_fd: std::os::fd::RawFd) -> bool {
    use std::os::fd::AsRawFd;

    let Ok(current) = open_unix_directory(path) else {
        return false;
    };
    unix_fd_identity(current.as_raw_fd())
        .zip(unix_fd_identity(expected_fd))
        .is_some_and(|(current, expected)| current == expected)
}

#[cfg(unix)]
fn unix_fd_identity(fd: std::os::fd::RawFd) -> Option<(u64, u64)> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    Some((stat.st_dev as u64, stat.st_ino as u64))
}

#[cfg(unix)]
fn unix_directory_open_flags() -> libc::c_int {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let access = libc::O_PATH;
    #[cfg(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "solaris",
        target_os = "illumos",
        target_os = "aix",
        target_os = "fuchsia"
    ))]
    let access = libc::O_SEARCH;
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "solaris",
        target_os = "illumos",
        target_os = "aix",
        target_os = "fuchsia"
    )))]
    let access = libc::O_RDONLY;

    access | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC
}

#[cfg(windows)]
pub(crate) fn open_regular_file_windows(
    path: &Path,
    before_leaf: impl FnOnce(),
) -> io::Result<File> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_SHARE_READ};

    let parent_path = path
        .parent()
        .ok_or_else(|| io::Error::other("instruction path must name an absolute file"))?;
    let parent = match open_windows_directory(parent_path) {
        Ok(handle) => File::from(handle),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::other("instruction parent is unavailable"));
        }
        Err(error) => return Err(error),
    };
    let parent_identity = windows_file_identity(&parent)?;
    let leaf = path
        .file_name()
        .ok_or_else(|| io::Error::other("instruction path must name an absolute file"))?;

    before_leaf();

    let file = match windows_nt_open_relative(
        parent.as_raw_handle() as HANDLE,
        leaf,
        FILE_GENERIC_READ,
        FILE_SHARE_READ,
        WINDOWS_FILE_NON_DIRECTORY_FILE,
    ) {
        Ok(handle) => File::from(handle),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !windows_parent_still_current(parent_path, parent_identity) {
                return Err(io::Error::other(
                    "instruction parent changed during observation",
                ));
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata_is_link_like(&metadata) {
        return Err(io::Error::other(
            "instruction handle is not an ordinary file",
        ));
    }
    if !windows_parent_still_current(parent_path, parent_identity) {
        return Err(io::Error::other(
            "instruction parent changed during observation",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn windows_parent_still_current(path: &Path, expected: (u64, [u8; 16])) -> bool {
    open_windows_directory(path)
        .map(File::from)
        .and_then(|file| windows_file_identity(&file))
        .is_ok_and(|current| current == expected)
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> io::Result<(u64, [u8; 16])> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let mut info = std::mem::MaybeUninit::<FILE_ID_INFO>::zeroed();
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle().cast(),
            FileIdInfo,
            info.as_mut_ptr().cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

#[cfg(windows)]
fn open_windows_directory(path: &Path) -> io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::path::Component;
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let mut components = path.components();
    let prefix = match components.next() {
        Some(Component::Prefix(prefix)) => prefix,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "instruction path must be an absolute Windows path",
            ))
        }
    };
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "instruction path must include a Windows root",
        ));
    }

    let mut root = std::path::PathBuf::from(prefix.as_os_str());
    root.push(r"\");
    let wide = std::os::windows::ffi::OsStrExt::encode_wide(root.as_os_str())
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // Resolve only the Windows drive/share root through Win32 namespace rules.
    // Every real filesystem descendant is then opened relative to the pinned
    // parent handle with OBJ_DONT_REPARSE, so junction/reparse ancestors remain
    // fail-closed without rejecting the ordinary DOS drive mapping itself.
    let root_handle: HANDLE = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if root_handle.is_null() || root_handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut directory = unsafe { OwnedHandle::from_raw_handle(root_handle as RawHandle) };

    for component in components {
        let name = match component {
            Component::Normal(name) => name,
            Component::CurDir => continue,
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "instruction path must not contain parent traversal",
                ))
            }
        };
        directory = windows_nt_open_relative(
            directory.as_raw_handle() as HANDLE,
            name,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            WINDOWS_FILE_DIRECTORY_FILE,
        )?;
    }

    Ok(directory)
}

#[cfg(windows)]
fn windows_nt_open_relative(
    root: windows_sys::Win32::Foundation::HANDLE,
    name: &std::ffi::OsStr,
    desired_access: u32,
    share_access: u32,
    create_options: u32,
) -> io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::ffi::OsStrExt;

    let mut name = name.encode_wide().collect::<Vec<_>>();
    windows_nt_open(
        root,
        &mut name,
        desired_access,
        share_access,
        create_options,
    )
}

#[cfg(windows)]
fn windows_nt_open(
    root: windows_sys::Win32::Foundation::HANDLE,
    name: &mut [u16],
    desired_access: u32,
    share_access: u32,
    create_options: u32,
) -> io::Result<std::os::windows::io::OwnedHandle> {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};
    use std::ptr;
    use windows_sys::Win32::Foundation::{
        RtlNtStatusToDosError, HANDLE, INVALID_HANDLE_VALUE, NTSTATUS, UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    const FILE_OPEN: u32 = 1;
    const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
    const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;

    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root_directory: HANDLE,
        object_name: *const UNICODE_STRING,
        attributes: u32,
        security_descriptor: *const c_void,
        security_quality_of_service: *const c_void,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            file_handle: *mut HANDLE,
            desired_access: u32,
            object_attributes: *const ObjectAttributes,
            io_status_block: *mut IO_STATUS_BLOCK,
            allocation_size: *const i64,
            file_attributes: u32,
            share_access: u32,
            create_disposition: u32,
            create_options: u32,
            ea_buffer: *const c_void,
            ea_length: u32,
        ) -> NTSTATUS;
    }

    let name_length = u16::try_from(name.len().saturating_mul(size_of::<u16>()))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "instruction path is too long"))?;
    let object_name = UNICODE_STRING {
        Length: name_length,
        MaximumLength: name_length,
        Buffer: name.as_mut_ptr(),
    };
    let object_attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root_directory: root,
        object_name: &object_name,
        attributes: WINDOWS_OBJ_CASE_INSENSITIVE | WINDOWS_OBJ_DONT_REPARSE,
        security_descriptor: ptr::null(),
        security_quality_of_service: ptr::null(),
    };
    let mut io_status = std::mem::MaybeUninit::<IO_STATUS_BLOCK>::zeroed();
    let mut handle: HANDLE = std::ptr::null_mut();
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access | SYNCHRONIZE_ACCESS,
            &object_attributes,
            io_status.as_mut_ptr(),
            ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            share_access,
            FILE_OPEN,
            create_options | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    if status < 0 {
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::other(
            "NtCreateFile returned an invalid instruction handle",
        ));
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) })
}
