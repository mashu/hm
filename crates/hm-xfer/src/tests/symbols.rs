//! Symbol sizes: no larger than the object needs, smaller on slow links.

use super::*;

#[test]
fn symbols_are_no_larger_than_the_object_needs() {
    // One symbol: the object rounded up to the 8-byte alignment.
    assert_eq!(fit_symbol(119, 200), 120);
    assert_eq!(fit_symbol(1, 200), 8);
    // Several: as few as the largest size allows, each as small as that permits.
    assert_eq!(fit_symbol(1000, 200), 200);
    assert_eq!(fit_symbol(1010, 200), 176); // 6 x 176 = 1056 bytes on air, not 6 x 200
    assert_eq!(fit_symbol(1500, 200), 192);
    for len in 1..3000u32 {
        for max in [8u16, 64, 200, 256] {
            let t = fit_symbol(len, max);
            assert!(t.is_multiple_of(8) && (8..=max).contains(&t), "{len} {max}: {t}");
            assert_eq!(
                len.div_ceil(u32::from(t)),
                len.div_ceil(u32::from(max)),
                "{len} {max}: no more symbols than the largest size needs"
            );
        }
    }
}

/// A 119-byte chat bundle goes out as one 120-byte symbol: the 81 bytes of
/// padding a 200-byte symbol would carry are airtime for nothing.
#[test]
fn a_short_message_is_not_padded_to_the_full_symbol_size() {
    let mut a = engine("SA0KAM");
    let mut b = engine("SO5KM-1");
    let object = vec![0x33u8; 119];
    let burst = frames(&send(&mut a, Millis(0), "SO5KM-1", object.clone()));
    let data: Vec<&Vec<u8>> = burst
        .iter()
        .filter(|f| FrameHeader::decode(f).unwrap().0.ftype == FrameType::Data)
        .collect();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].len(), HEADER_LEN + DATA_PREAMBLE_LEN + 120);
    let got = events(&deliver(&mut b, Millis(5_000), &burst));
    assert!(got.contains(&Event::Received {
        from: call("SA0KAM"),
        id: object_id(&object),
        object,
    }));
}

#[test]
fn slow_links_get_smaller_symbols_and_shorter_overs() {
    let me = call("SA0KAM");
    let vhf = Config::for_link(me, 1200, Millis(300));
    assert_eq!(vhf, Config::vhf_1200(me));
    let hf = Config::hf_300(me);
    // 32-byte symbols: a DATA frame of 73 bytes on air, 2 s at 300 bd.
    assert_eq!(hf.symbol_size, 32);
    assert_eq!(hf.max_over, SLOW_MAX_OVER);
    let frame = hf.air(1, hf.data_frame_len(32));
    assert!(frame >= Millis(1_900) && frame <= Millis(2_100), "{frame:?}");
    assert_eq!(Config::for_link(me, 600, Millis(300)).symbol_size, 104);
    assert_eq!(Config::for_link(me, 9600, Millis(100)).symbol_size, 200);
    for bitrate in [50, 75, 110, 300, 600, 1200, 2400, 9600] {
        let c = Config::for_link(me, bitrate, Millis(300));
        assert!(c.validate().is_ok(), "{bitrate}");
    }
}
