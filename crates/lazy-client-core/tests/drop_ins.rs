//! Drop-ins in the core: one at a time, its state follows the runtime's reports for the current
//! join only, and signing out leaves it.

mod common;

use common::{Driver, World};
use lazy_client_core::{
    Change, DeviceChoices, DeviceKind, DropInPeer, DropInReport, DropInStatus, Effect, Input,
    MediaDevice, MediaDevices,
};

fn drop_in_effects(driver: &Driver) -> Vec<Effect> {
    driver
        .effects
        .iter()
        .filter(|e| {
            matches!(
                e,
                Effect::JoinDropIn { .. }
                    | Effect::LeaveDropIn { .. }
                    | Effect::SetDropInMuted { .. }
                    | Effect::SetDropInVideo { .. }
                    | Effect::SetDropInDevice { .. }
            )
        })
        .cloned()
        .collect()
}

fn peer(id: u64, name: &str) -> DropInPeer {
    DropInPeer {
        id,
        user: None,
        name: name.into(),
        muted: false,
        video_on: true,
    }
}

#[test]
fn joining_follows_the_call_and_leaving_ends_it() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let channel = world.channel.id;

    driver.send(Input::JoinDropIn(channel));
    driver.send(Input::JoinDropIn(channel));
    assert_eq!(
        drop_in_effects(&driver),
        [Effect::JoinDropIn {
            session: 1,
            conversation: channel,
            muted: false,
            video_on: true,
            devices: DeviceChoices::default(),
        }],
        "joined once"
    );
    let drop_in = driver.core.drop_in().unwrap();
    assert_eq!(drop_in.status, DropInStatus::Joining);
    assert!(driver.changes.contains(&Change::DropInChanged));

    driver.send(Input::DropInReport {
        session: 1,
        report: DropInReport::Joined,
    });
    assert_eq!(
        driver.core.drop_in().unwrap().status,
        DropInStatus::Connecting
    );
    driver.send(Input::DropInReport {
        session: 1,
        report: DropInReport::Connected,
    });
    driver.send(Input::DropInReport {
        session: 1,
        report: DropInReport::Peers(vec![peer(2, "Bo")]),
    });
    driver.send(Input::DropInReport {
        session: 1,
        report: DropInReport::Speaking(vec![None, Some(2)]),
    });
    let drop_in = driver.core.drop_in().unwrap();
    assert_eq!(drop_in.status, DropInStatus::Live);
    assert_eq!(drop_in.peers, [peer(2, "Bo")]);
    assert!(drop_in.is_speaking(Some(2)) && drop_in.is_speaking(None));

    // Muting stops showing this person as speaking; the camera can go off.
    driver.send(Input::SetDropInMuted(true));
    driver.send(Input::SetDropInVideo(false));
    let drop_in = driver.core.drop_in().unwrap();
    assert!(drop_in.muted && !drop_in.video_on && !drop_in.is_speaking(None));

    // Joining another conversation's drop-in leaves this one first.
    driver.send(Input::JoinDropIn(world.second_channel.id));
    let effects = drop_in_effects(&driver);
    assert_eq!(
        &effects[effects.len() - 2..],
        [
            Effect::LeaveDropIn { session: 1 },
            Effect::JoinDropIn {
                session: 2,
                conversation: world.second_channel.id,
                muted: false,
                video_on: true,
                devices: DeviceChoices::default(),
            }
        ]
    );
    // A late report from the first call is ignored.
    driver.send(Input::DropInReport {
        session: 1,
        report: DropInReport::Ended(Some("lost".into())),
    });
    assert_eq!(
        driver.core.drop_in().unwrap().conversation,
        world.second_channel.id
    );
    assert_eq!(driver.core.drop_in_error(), None);

    driver.send(Input::LeaveDropIn);
    assert!(driver.core.drop_in().is_none());
    assert_eq!(
        drop_in_effects(&driver).last(),
        Some(&Effect::LeaveDropIn { session: 2 })
    );
}

#[test]
fn a_failed_call_says_why_until_dismissed() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    driver.send(Input::JoinDropIn(world.channel.id));
    driver.send(Input::DropInReport {
        session: 1,
        report: DropInReport::DeviceProblem("Camera: no camera was found".into()),
    });
    let drop_in = driver.core.drop_in().unwrap();
    assert!(!drop_in.video_on, "no camera means video is off");
    assert_eq!(
        drop_in.problem.as_deref(),
        Some("Camera: no camera was found")
    );

    driver.send(Input::DropInReport {
        session: 1,
        report: DropInReport::Ended(Some("Drop-ins aren't set up on this server".into())),
    });
    assert!(driver.core.drop_in().is_none());
    assert_eq!(
        driver.core.drop_in_error(),
        Some("Drop-ins aren't set up on this server")
    );
    driver.send(Input::DismissDropInError);
    assert_eq!(driver.core.drop_in_error(), None);
}

#[test]
fn signing_out_leaves_the_drop_in() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    driver.send(Input::JoinDropIn(world.channel.id));
    driver.send(Input::SignOut);
    assert!(driver.core.drop_in().is_none());
    assert!(drop_in_effects(&driver).contains(&Effect::LeaveDropIn { session: 1 }));
}

#[test]
fn device_choices_are_saved_passed_to_the_call_and_switch_it() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let brio = Some("Logitech BRIO".to_owned());

    // Outside a call, a choice is only saved.
    driver.send(Input::ChooseDevice {
        kind: DeviceKind::Camera,
        id: brio.clone(),
    });
    assert_eq!(driver.core.device_choices().camera, brio);
    let saved = DeviceChoices {
        camera: brio.clone(),
        ..DeviceChoices::default()
    };
    assert!(
        driver
            .effects
            .contains(&Effect::PersistDeviceChoices(saved.clone()))
    );
    assert!(driver.changes.contains(&Change::MediaDevicesChanged));
    assert!(drop_in_effects(&driver).is_empty(), "no call to switch");

    // Joining hands the choices to the call, and lists the devices for the menu.
    driver.send(Input::JoinDropIn(world.channel.id));
    assert!(driver.effects.iter().any(|e| matches!(
        e,
        Effect::JoinDropIn { devices, .. } if *devices == saved
    )));
    assert!(driver.effects.contains(&Effect::ListMediaDevices));

    // In a call, a choice switches it; choosing the same again does nothing.
    let mic = Some("mic-2".to_owned());
    driver.send(Input::ChooseDevice {
        kind: DeviceKind::Microphone,
        id: mic.clone(),
    });
    assert_eq!(
        drop_in_effects(&driver).last(),
        Some(&Effect::SetDropInDevice {
            session: 1,
            kind: DeviceKind::Microphone,
            id: mic.clone(),
        })
    );
    let before = driver.effects.len();
    driver.send(Input::ChooseDevice {
        kind: DeviceKind::Microphone,
        id: mic,
    });
    assert_eq!(driver.effects.len(), before);

    // Back to automatic.
    driver.send(Input::ChooseDevice {
        kind: DeviceKind::Camera,
        id: None,
    });
    assert_eq!(driver.core.device_choices().camera, None);

    // The listing is kept as given.
    let devices = MediaDevices {
        cameras: vec![MediaDevice {
            id: "Logitech BRIO".into(),
            name: "Logitech BRIO".into(),
        }],
        ..MediaDevices::default()
    };
    driver.send(Input::MediaDevicesListed(devices.clone()));
    assert_eq!(driver.core.media_devices(), &devices);

    // Choices belong to the device, not the account: signing out keeps them.
    driver.send(Input::SignOut);
    assert_eq!(
        driver.core.device_choices().microphone.as_deref(),
        Some("mic-2")
    );
}
