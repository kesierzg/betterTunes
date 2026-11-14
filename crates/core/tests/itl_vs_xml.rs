//! Checks the .itl reader against iTunes' own XML export of the same library.
//!
//! Needs a real library, so it is ignored by default:
//!
//!     BT_ITL="…/iTunes Library.itl" BT_XML="…/iTunes Library.xml" \
//!         cargo test -p bettertunes-core --test itl_vs_xml -- --ignored

use bettertunes_core::Library;
use std::collections::HashMap;
use std::path::Path;

#[test]
#[ignore = "needs BT_ITL and BT_XML pointing at a real library"]
fn itl_matches_xml_export() {
    let itl = Library::open(Path::new(&std::env::var("BT_ITL").unwrap())).unwrap();
    let xml = Library::open(Path::new(&std::env::var("BT_XML").unwrap())).unwrap();

    assert_eq!(itl.tracks.len(), xml.tracks.len());
    let by_id: HashMap<_, _> = itl.tracks.iter().map(|t| (t.persistent_id, t)).collect();
    for x in &xml.tracks {
        let t = by_id
            .get(&x.persistent_id)
            .unwrap_or_else(|| panic!("track {:016X} missing from .itl", x.persistent_id));
        assert_eq!(*t, x, "track {:016X} differs", x.persistent_id);
    }

    // The XML leaves out some internal playlists, so check that each XML
    // playlist has an identical .itl counterpart. The master playlist is
    // "####!####" in the .itl but "Library" in the XML.
    for x in &xml.playlists {
        assert!(
            itl.playlists
                .iter()
                .any(|p| (p.name == x.name || p.is_master && x.is_master)
                    && p.persistent_id == x.persistent_id
                    && p.track_ids == x.track_ids
                    && p.is_smart == x.is_smart
                    && p.distinguished_kind == x.distinguished_kind),
            "playlist {:?} has no identical counterpart in .itl\n xml: {x:?}\n itl: {:?}",
            x.name,
            itl.playlists
                .iter()
                .find(|p| p.persistent_id == x.persistent_id)
        );
    }
}
