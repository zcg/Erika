pub const FFMPEG_VERSION: &str = "8.1.2";
pub const LIBASS_VERSION: &str = "0.17.5";
pub const HARFBUZZ_VERSION: &str = "14.2.1";
pub const FREETYPE_VERSION: &str = "2.14.3";

#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unnecessary_transmutes
)]
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

pub use bindings::*;

pub const ERIKA_SWS_BILINEAR: std::os::raw::c_int = SwsFlags_SWS_BILINEAR as std::os::raw::c_int;
pub const ERIKA_PROFILE_UNKNOWN: i32 = AV_PROFILE_UNKNOWN;

#[cfg(test)]
mod tests {
    use super::{ERIKA_PROFILE_UNKNOWN, ERIKA_SWS_BILINEAR};

    #[test]
    fn ffmpeg_812_compatibility_constants() {
        assert_eq!(ERIKA_SWS_BILINEAR, 2);
        assert_eq!(ERIKA_PROFILE_UNKNOWN, -99);
    }
}
