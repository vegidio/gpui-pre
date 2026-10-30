//! `gpui::DmaBufTexture`, tested here because `gpui-pre`'s own unit tests need Zed's repository to build.
#![cfg(target_os = "linux")]

use gpui::{DmaBufLayout, DmaBufTexture};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;

const LAYOUT: DmaBufLayout = DmaBufLayout {
    width: 2,
    height: 2,
    fourcc: 0x3432_4241,
    modifier: 0,
    stride: 8,
    offset: 0,
};

#[test]
fn duplicate_outlives_the_original() {
    let original = std::fs::File::open("/proc/self/exe").unwrap();
    let inode = original.metadata().unwrap().ino();
    let texture = DmaBufTexture::duplicate(original.as_fd(), LAYOUT).unwrap();
    drop(original);

    let reopened = std::fs::File::from(texture.fd().try_clone_to_owned().unwrap());
    assert_eq!(
        reopened.metadata().unwrap().ino(),
        inode,
        "the duplicate still refers to the file"
    );
    assert_eq!(texture.layout(), LAYOUT);
}

#[test]
fn clones_are_equal_and_duplicates_are_not() {
    let file = std::fs::File::open("/proc/self/exe").unwrap();
    let first = DmaBufTexture::duplicate(file.as_fd(), LAYOUT).unwrap();
    let second = DmaBufTexture::duplicate(file.as_fd(), LAYOUT).unwrap();
    assert_eq!(first.clone(), first);
    assert_ne!(first, second);
}
