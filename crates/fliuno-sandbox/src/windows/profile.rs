use super::{path_wide, wide};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Authorization::ConvertSidToStringSidW,
        FreeSid,
        Isolation::{
            CreateAppContainerProfile, DeleteAppContainerProfile, GetAppContainerFolderPath,
        },
        PSID,
    },
    System::Com::CoTaskMemFree,
};

pub(super) struct Profile {
    name: Vec<u16>,
    // Store the SID spelling, not a raw pointer, so the owner is Send + Sync.
    sid: Vec<u32>,
    sid_string: String,
    directory: PathBuf,
}

impl Profile {
    pub(super) fn create() -> io::Result<Self> {
        let name = wide(&format!("fliuno.{}", uuid::Uuid::new_v4().simple()))?;
        let mut raw_sid = std::ptr::null_mut();
        // SAFETY: strings are terminated, no capabilities; output receives a SID.
        let result = unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                name.as_ptr(),
                name.as_ptr(),
                std::ptr::null(),
                0,
                &mut raw_sid,
            )
        };
        if result < 0 {
            return Err(io::Error::other(format!(
                "CreateAppContainerProfile failed: 0x{result:08x}"
            )));
        }
        let mut sid_text = std::ptr::null_mut();
        // SAFETY: raw_sid was returned by successful profile creation.
        let converted = unsafe { ConvertSidToStringSidW(raw_sid, &mut sid_text) };
        if converted == 0 {
            let error = io::Error::last_os_error();
            // SAFETY: release both resources from the successful profile creation.
            unsafe {
                FreeSid(raw_sid);
                DeleteAppContainerProfile(name.as_ptr());
            }
            return Err(error);
        }
        // SAFETY: sid_text is a successful API allocation terminated by NUL.
        let sid_string = unsafe { read_wide(sid_text) };
        // SAFETY: the SID is valid; length is supplied by the API for this buffer.
        let sid_len = unsafe { windows_sys::Win32::Security::GetLengthSid(raw_sid) } as usize;
        // SAFETY: raw_sid points at sid_len initialized bytes. Copy before FreeSid.
        let mut sid = vec![0u32; sid_len.div_ceil(std::mem::size_of::<u32>())];
        // SAFETY: copy exactly the SID size into a sufficiently sized DWORD-
        // aligned allocation. Every u32 bit pattern is valid.
        unsafe {
            std::ptr::copy_nonoverlapping(
                raw_sid.cast::<u8>(),
                sid.as_mut_ptr().cast::<u8>(),
                sid_len,
            );
        }
        // SAFETY: each allocation is freed once with its matching allocator.
        unsafe {
            FreeSid(raw_sid);
            LocalFree(sid_text.cast());
        }
        let mut this = Self {
            name,
            sid,
            sid_string,
            directory: PathBuf::new(),
        };
        let sid_wide = wide(&this.sid_string)?;
        let mut folder = std::ptr::null_mut();
        // SAFETY: valid terminated SID string and a valid output pointer.
        let result = unsafe { GetAppContainerFolderPath(sid_wide.as_ptr(), &mut folder) };
        if result < 0 {
            return Err(io::Error::other(format!(
                "GetAppContainerFolderPath failed: 0x{result:08x}"
            )));
        }
        // SAFETY: the API returned a NUL-terminated CoTaskMem allocation.
        let directory = unsafe { read_wide(folder) };
        // SAFETY: matching deallocator after copying the returned string.
        unsafe {
            CoTaskMemFree(folder.cast());
        }
        this.directory = PathBuf::from(directory).join("LocalState").join("fliuno");
        path_wide(&this.directory)?;
        fs::create_dir_all(&this.directory)?;
        Ok(this)
    }
    pub(super) fn sid(&self) -> PSID {
        self.sid.as_ptr().cast_mut().cast()
    }
    pub(super) fn sid_string(&self) -> &str {
        &self.sid_string
    }
    pub(super) fn directory(&self) -> &Path {
        &self.directory
    }
    pub(super) fn stage_program(
        &self,
        program: &Path,
        expected: Option<&str>,
    ) -> io::Result<PathBuf> {
        let path = self.directory.join("program.exe");
        crate::program::stage(program, &path, expected)?;
        Ok(path)
    }
}
impl Drop for Profile {
    fn drop(&mut self) {
        // SAFETY: profile name is uniquely generated and this owner deletes it once.
        let result = unsafe { DeleteAppContainerProfile(self.name.as_ptr()) };
        if result < 0 {
            eprintln!("fliuno: cannot remove sandbox profile: 0x{result:08x}");
        }
    }
}

unsafe fn read_wide(raw: *const u16) -> String {
    let mut length = 0;
    // SAFETY: callers pass a successfully returned NUL-terminated API string.
    unsafe {
        while *raw.add(length) != 0 {
            length += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(raw, length))
    }
}
