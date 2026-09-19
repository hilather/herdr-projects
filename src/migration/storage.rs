//! Read-only capacity/type checks. A supported mount type is not a hardware certification.
use anyhow::{Result,Context};
use serde::Serialize;
use std::{ffi::CString,os::unix::ffi::OsStrExt,path::Path};
#[derive(Debug,Serialize)]
pub struct Storage { pub destination:String,pub available_bytes:u64,pub required_bytes:u64,pub filesystem:String,pub supported_type:bool,pub sufficient_space:bool }
pub fn inspect(path:&Path,source_bytes:u64)->Result<Storage> {
    let destination=path.display().to_string();
    let path=CString::new(path.as_os_str().as_bytes()).context("NUL in project path")?;
    let mut stat=std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: valid NUL-terminated path and writable statvfs output allocation.
    if unsafe{libc::statvfs(path.as_ptr(),stat.as_mut_ptr())}!=0{return Err(std::io::Error::last_os_error().into());}
    // SAFETY: successful statvfs initialized the whole output.
    let stat=unsafe{stat.assume_init()};
    let available_bytes=(stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
    let required_bytes=source_bytes.checked_mul(4).and_then(|n|n.checked_add(64*1024*1024)).context("migration capacity estimate overflow")?;
    #[cfg(target_os="linux")]
    let(filesystem,supported_type)={
        let mut info=std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: valid path and output pointer as above.
        if unsafe{libc::statfs(path.as_ptr(),info.as_mut_ptr())}!=0{return Err(std::io::Error::last_os_error().into());}
        // SAFETY: successful statfs initialized output.
        let magic=unsafe{info.assume_init()}.f_type as u64;
        let name=match magic {0xef53=>"ext",0x9123683e=>"btrfs",0x58465342=>"xfs",0x01021994=>"tmpfs",0x794c7630=>"overlay",0x2fc12fc1=>"zfs",_=>"unverified"};
        (format!("{name} (0x{magic:x})"),name!="unverified")
    };
    #[cfg(not(target_os="linux"))]
    let(filesystem,supported_type)=("unverified platform".into(),false);
    Ok(Storage{destination,available_bytes,required_bytes,filesystem,supported_type,sufficient_space:available_bytes>=required_bytes})
}
