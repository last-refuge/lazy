//! Prints the cameras, microphones and speakers this machine offers, as a call would list them:
//!
//!     cargo run -p rtc-client --features devices --example devices
fn main() {
    println!("cameras: {:#?}", rtc_video::source::cameras());
    println!("microphones: {:#?}", rtc_audio::microphones());
    println!("speakers: {:#?}", rtc_audio::speakers());
}
