//! Link previews in the core (D41): asked for once per message and revision, kept, and shown only
//! for the revision they belong to.

mod common;

use common::{Driver, World};
use lazy_client_core::{Change, Effect, Input, RequestError};
use lazy_domain::MessageId;
use lazy_protocol::LinkPreview;

fn preview(title: &str) -> LinkPreview {
    LinkPreview {
        url: "https://miro.com/app/board/x".into(),
        title: Some(title.into()),
        description: None,
        site_name: Some("Miro".into()),
    }
}

fn asked(driver: &Driver) -> Vec<(MessageId, u32)> {
    driver
        .effects
        .iter()
        .filter_map(|e| match e {
            Effect::LoadPreviews { message, revision } => Some((*message, *revision)),
            _ => None,
        })
        .collect()
}

#[test]
fn previews_are_asked_for_once_per_revision_and_kept() {
    let world = World::new();
    let mut driver = Driver::ready(&world);
    let message = MessageId::generate();
    driver.send(Input::LoadPreviews {
        message,
        revision: 1,
    });
    driver.send(Input::LoadPreviews {
        message,
        revision: 1,
    });
    assert_eq!(asked(&driver), [(message, 1)]);
    assert!(driver.core.previews_requested(message, 1));
    assert_eq!(driver.core.link_previews(message, 1), None, "on its way");

    driver.send(Input::PreviewsLoaded {
        message,
        revision: 1,
        result: Ok(vec![preview("Q4 plan")]),
    });
    assert_eq!(
        driver.core.link_previews(message, 1),
        Some(&[preview("Q4 plan")][..])
    );
    assert!(
        driver
            .changes
            .contains(&Change::PreviewsChanged { message })
    );

    // An edit asks again; the old revision's previews are not shown for it.
    driver.send(Input::LoadPreviews {
        message,
        revision: 2,
    });
    assert_eq!(asked(&driver), [(message, 1), (message, 2)]);
    assert_eq!(driver.core.link_previews(message, 2), None);
    // A late answer for the old revision is dropped.
    driver.send(Input::PreviewsLoaded {
        message,
        revision: 1,
        result: Ok(vec![preview("old")]),
    });
    assert_eq!(driver.core.link_previews(message, 2), None);
    // A failure is no previews, and is not asked again for the same revision.
    driver.send(Input::PreviewsLoaded {
        message,
        revision: 2,
        result: Err(RequestError::Unknown("timed out".into())),
    });
    assert_eq!(driver.core.link_previews(message, 2), Some(&[][..]));
    driver.send(Input::LoadPreviews {
        message,
        revision: 2,
    });
    assert_eq!(asked(&driver).len(), 2);
}
