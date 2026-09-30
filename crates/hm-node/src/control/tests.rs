use hm_ident::Identity;
use hm_wire::{ContactBearer, Dest, FLAG_HOLDING, MAX_OFFER};

use super::trickle::DueAdvert;
use super::*;
use std::path::PathBuf;

fn call(value: &str) -> Callsign {
    value.parse().unwrap()
}

struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "hm-control-{name}-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        Self(path)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn advert(sequence: u32) -> ContactAdvert {
    ContactAdvert {
        origin: call("M0AAA"),
        sequence,
        start: 1_700_000_000,
        end: 2_000_000_000,
        peer: call("M0BBB"),
        bearer: ContactBearer::Radio,
        success_permyriad: 8_000,
        rate_bps: 1_200,
        capacity_bytes: 4_096,
        flags: 3,
        signature: [0; 64],
    }
}

#[test]
fn contacts_are_signed_and_changes_fail_verification() {
    let identity = Identity::from_secret([7; 32]);
    let mut contact = sign_contact(&identity, advert(1)).unwrap();
    assert!(verify_contact(&contact, identity.public()));
    contact.capacity_bytes += 1;
    assert!(!verify_contact(&contact, identity.public()));
}

fn due(advert: ContactAdvert, on_air: bool, internet: bool) -> DueAdvert {
    DueAdvert {
        advert,
        on_air,
        internet,
    }
}

#[test]
fn trickle_suppresses_duplicates_and_slows_when_stable() {
    let mut trickle = Trickle::new(100, 800, 2).unwrap();
    let contact = advert(1);
    assert!(trickle.observe(contact.clone(), 0, AdvertSource::Radio).unwrap());
    let first = trickle.next_deadline().unwrap();
    assert!((50..100).contains(&first));
    assert!(!trickle
        .observe(contact.clone(), first - 1, AdvertSource::Radio)
        .unwrap());
    assert!(!trickle
        .observe(contact.clone(), first - 1, AdvertSource::Radio)
        .unwrap());
    assert_eq!(
        trickle.poll(first),
        vec![due(contact.clone(), false, true)],
        "two copies on air silence the radio, not the internet"
    );
    assert!(trickle.poll(100).is_empty());
    let second = trickle.next_deadline().unwrap();
    assert!((200..300).contains(&second));
    assert_eq!(trickle.poll(second), vec![due(contact.clone(), true, true)]);
    let mut changed = advert(2);
    changed.success_permyriad = 5_000;
    assert!(trickle.observe(changed, second + 1, AdvertSource::Radio).unwrap());
    assert!(
        trickle.next_deadline().unwrap() < second + 101,
        "new content starts over"
    );
}

#[test]
fn a_refresh_that_says_the_same_is_passed_on_without_starting_over() {
    let mut trickle = Trickle::new(100, 800, 2).unwrap();
    trickle.observe(advert(1), 0, AdvertSource::Radio).unwrap();
    let first = trickle.next_deadline().unwrap();
    trickle.poll(first);
    trickle.poll(100);
    let second = trickle.next_deadline().unwrap();
    let mut refreshed = advert(9);
    refreshed.end += 3600;
    assert!(!trickle
        .observe(refreshed.clone(), 101, AdvertSource::Own { on_air: true })
        .unwrap());
    assert_eq!(
        trickle.next_deadline(),
        Some(second),
        "the interval keeps growing"
    );
    assert_eq!(
        trickle.poll(second)[0].advert,
        refreshed,
        "the newest copy goes out"
    );
    assert!(!trickle
        .observe(advert(3), second + 1, AdvertSource::Radio)
        .unwrap());
    assert_eq!(
        trickle.adverts().next().unwrap().sequence,
        9,
        "an older copy never replaces it"
    );
}

#[test]
fn stations_that_hear_an_advert_together_pick_different_moments() {
    let moments: std::collections::BTreeSet<u64> = ["M0AAA", "M0BBB", "M0CCC", "M0DDD", "M0EEE"]
        .iter()
        .map(|me| {
            let mut trickle = Trickle::new(5_000, 60_000, 2)
                .unwrap()
                .with_salt(call(me).packed());
            trickle.observe(advert(1), 1_000, AdvertSource::Radio).unwrap();
            trickle.next_deadline().unwrap()
        })
        .collect();
    assert_eq!(moments.len(), 5);
}

#[test]
fn only_radio_contacts_heard_on_air_or_claimed_for_it_go_on_air() {
    let mut trickle = Trickle::new(100, 800, 2).unwrap();
    let mut from_internet = advert(1);
    from_internet.peer = call("M0CCC");
    let mut internet_link = advert(1);
    internet_link.peer = call("M0DDD");
    internet_link.bearer = ContactBearer::Internet;
    let mut own_live = advert(1);
    own_live.peer = call("M0EEE");
    trickle.observe(advert(1), 0, AdvertSource::Radio).unwrap();
    trickle
        .observe(from_internet.clone(), 0, AdvertSource::Internet)
        .unwrap();
    trickle
        .observe(internet_link.clone(), 0, AdvertSource::Radio)
        .unwrap();
    trickle
        .observe(own_live.clone(), 0, AdvertSource::Own { on_air: false })
        .unwrap();
    let on_air: Vec<Callsign> = trickle
        .poll(99)
        .into_iter()
        .filter(|due| due.on_air)
        .map(|due| due.advert.peer)
        .collect();
    assert_eq!(on_air, vec![call("M0BBB")]);
    // Heard on the radio later: now it belongs there too.
    trickle.observe(from_internet, 150, AdvertSource::Radio).unwrap();
    assert!(trickle
        .poll(299)
        .iter()
        .any(|due| due.on_air && due.advert.peer == call("M0CCC")));
}

#[test]
fn the_sync_queue_sends_the_newest_and_never_the_expired() {
    let mut queue = SyncQueue::new(4);
    let contact = |sequence: u32, end: u32| {
        let mut a = advert(sequence);
        a.start = 10;
        a.end = end;
        SyncMessage::Contact(a).encode().unwrap()
    };
    queue.push(Dest::Broadcast, contact(1, 500), 100);
    queue.push(Dest::Broadcast, contact(2, 900), 100);
    assert_eq!(
        queue.len(),
        1,
        "a newer copy of the same contact takes the old one's place"
    );
    assert_eq!(queue.front(100).unwrap().payload, contact(2, 900));
    let filter = |salt: u32| {
        SyncMessage::Filter(holdings_filter(0, salt, &[]).unwrap())
            .encode()
            .unwrap()
    };
    queue.push(Dest::Station(call("M0BBB")), filter(1), 100);
    queue.push(Dest::Station(call("M0BBB")), filter(2), 110);
    queue.push(Dest::Station(call("M0CCC")), filter(3), 110);
    assert_eq!(queue.len(), 3);
    assert!(queue.front(899).is_some());
    assert!(queue.front(900).is_none(), "everything has expired");
    assert_eq!(queue.dropped(), 3);
}

#[test]
fn a_frame_longer_than_the_share_goes_out_after_enough_quiet() {
    // 0.05% of an hour is 1.8 s; a 4 s frame needs 8,000 s of quiet.
    let mut budget = ControlBudget::new(3_600_000, 200).unwrap();
    budget.set_permyriad(5);
    assert!(budget.admit(0, 4_000));
    assert!(!budget.admit(3_600_000, 4_000));
    assert!(!budget.admit(7_999_999, 1_000));
    assert!(budget.admit(8_000_000, 4_000));
}

#[test]
fn a_contact_heard_on_air_is_not_sent_again() {
    let mut queue = SyncQueue::new(4);
    let contact = |sequence: u32| SyncMessage::Contact(advert(sequence)).encode().unwrap();
    queue.push(Dest::Broadcast, contact(5), 100);
    queue.heard(&contact(4));
    assert_eq!(queue.len(), 1, "an older copy on air says less than ours");
    queue.heard(&contact(5));
    assert!(queue.is_empty());
}

#[test]
fn the_channel_budgets_are_shared_by_the_stations_on_it() {
    assert_eq!(control_share_permyriad(200, 1), 200);
    assert_eq!(control_share_permyriad(200, 10), 20);
    assert_eq!(control_share_permyriad(200, 1_000), 1);
    // Beacons of 2 s: 10 minutes for up to 6 stations, then longer.
    assert_eq!(beacon_interval_ms(600_000, 6, 2_000), 600_000);
    assert_eq!(beacon_interval_ms(600_000, 40, 2_000), 4_000_000);
    assert_eq!(live_window_secs(600), 1_500);
    assert_eq!(live_window_secs(60), 1_200);
    assert!(radio_pull_due(FLAG_HOLDING, None, 0));
    assert!(!radio_pull_due(0, None, 0), "nothing held, nothing to pull");
    assert!(!radio_pull_due(FLAG_HOLDING, Some(0), RADIO_PULL_SECS - 1));
    assert!(radio_pull_due(FLAG_HOLDING, Some(0), RADIO_PULL_SECS));
}

#[test]
fn rolling_budget_never_exceeds_two_percent() {
    let mut budget = ControlBudget::new(1_000, 200).unwrap();
    assert!(budget.admit(0, 12));
    assert!(budget.admit(1, 8));
    assert!(!budget.admit(2, 1));
    assert_eq!(budget.used_ms(999), 20);
    assert!(budget.admit(1_000, 12));
    assert_eq!(budget.used_ms(1_000), 20);
    assert!(budget.admit(1_001, 8));
}

#[test]
fn filter_avoids_known_ids_and_offers_missing_ids_in_pages() {
    let ids: Vec<ObjectId> = (0..20)
        .map(|value| ObjectId(*blake3::hash(&[value]).as_bytes()))
        .collect();
    let filter = holdings_filter(0, 7, &ids[..2]).unwrap();
    let offers = offer_pages(&ids, &filter).unwrap();
    assert!(offers.len() <= 2);
    assert!(offers.iter().all(|offer| offer.prefixes.len() <= MAX_OFFER));
    assert!(!offers
        .iter()
        .flat_map(|offer| &offer.prefixes)
        .any(|prefix| ids[..2].iter().any(|id| id.prefix8() == *prefix)));
}

#[test]
fn pairwise_filter_offer_want_requests_only_an_offered_holding() {
    let (a, b) = (call("M0AAA"), call("M0BBB"));
    let (a_key, b_key) = (Identity::from_secret([1; 32]), Identity::from_secret([2; 32]));
    let mut a_trust = Trust::default();
    a_trust.insert(b, b_key.public());
    let mut b_trust = Trust::default();
    b_trust.insert(a, a_key.public());
    let (a_db, b_db) = (TempDb::new("a"), TempDb::new("b"));
    let (a_store, b_store) = (Store::open(&a_db.0).unwrap(), Store::open(&b_db.0).unwrap());
    let id = ObjectId(*blake3::hash(b"mail for B").as_bytes());
    a_store.enqueue(id, b"mail for B", b, 0, 10).unwrap();
    let mut a_plane = ControlPlane::default();
    let mut b_plane = ControlPlane::default();
    let mut a_graph = ContactGraph::new(Default::default()).unwrap();
    let mut b_graph = ContactGraph::new(Default::default()).unwrap();

    let filter = b_plane.filters(a, &b_store, false, 20).unwrap().remove(0);
    let offer = match a_plane
        .receive(
            b,
            &filter,
            &a_trust,
            None,
            (a, a_key.public()),
            &a_store,
            &mut a_graph,
            false,
            true,
            20,
        )
        .unwrap()
        .remove(0)
    {
        ControlAction::Reply(payload) => payload,
        action => panic!("unexpected action: {action:?}"),
    };
    let want = match b_plane
        .receive(
            a,
            &offer,
            &b_trust,
            None,
            (b, b_key.public()),
            &b_store,
            &mut b_graph,
            false,
            true,
            20,
        )
        .unwrap()
        .remove(0)
    {
        ControlAction::Reply(payload) => payload,
        action => panic!("unexpected action: {action:?}"),
    };
    assert_eq!(
        a_plane
            .receive(
                b,
                &want,
                &a_trust,
                None,
                (a, a_key.public()),
                &a_store,
                &mut a_graph,
                false,
                true,
                20,
            )
            .unwrap(),
        vec![ControlAction::Requested { id, peer: b }]
    );
    assert_eq!(a_plane.target_for(id, 20), Some(b));
}

#[test]
fn open_hub_accepts_sync_from_transport_authenticated_peer() {
    let (hub, home) = (call("SA0KAM-0"), call("SA0KAM-1"));
    let home_key = Identity::from_secret([3; 32]);
    let empty = Trust::default();
    let db = TempDb::new("hub-open");
    let store = Store::open(&db.0).unwrap();
    let mut plane = ControlPlane::default();
    let mut graph = ContactGraph::new(Default::default()).unwrap();
    let filter = ControlPlane::default()
        .filters(hub, &store, false, 20)
        .unwrap()
        .remove(0);
    assert!(plane
        .receive(
            home,
            &filter,
            &empty,
            None,
            (hub, Identity::from_secret([1; 32]).public()),
            &store,
            &mut graph,
            true,
            false,
            20,
        )
        .unwrap_err()
        .contains("untrusted SYNC peer"));
    let actions = plane
        .receive(
            home,
            &filter,
            &empty,
            Some(home_key.public()),
            (hub, Identity::from_secret([1; 32]).public()),
            &store,
            &mut graph,
            true,
            false,
            20,
        )
        .unwrap();
    // Empty store → no offers; admission itself is the assertion.
    assert!(actions.is_empty() || actions.iter().any(|a| matches!(a, ControlAction::Reply(_))));
}
