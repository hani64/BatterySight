//! HID++ request framing and reply matching.
//!
//! Report ids and error markers are the values the Linux kernel driver
//! `drivers/hid/hid-logitech-hidpp.c` uses: `REPORT_ID_HIDPP_SHORT` (0x10,
//! 7 bytes), `REPORT_ID_HIDPP_LONG` (0x11, 20 bytes), `HIDPP_ERROR` (0x8f,
//! the HID++ 1.0 error sub-id) and `HIDPP20_ERROR` (0xff, the HID++ 2.0
//! error feature index).
//!
//! Every frame is `[report_id, device_index, sub_id | feature_index,
//! address | function<<4 | software_id, params…]`. HID++ 2.0 devices echo the
//! requester's software id in the low nibble of byte 3; as Solaar does, we
//! send a fixed non-zero id so our replies can be told apart from other
//! software's and from device notifications (which carry software id 0).

/// Software id stamped into every HID++ 2.0 request we send.
pub const SW_ID: u8 = 0x0B;
/// Short HID++ report id.
pub const SHORT: u8 = 0x10;
/// Long HID++ report id.
pub const LONG: u8 = 0x11;

const SHORT_LEN: usize = 7;
const LONG_LEN: usize = 20;
const HEADER_LEN: usize = 4;
const HIDPP10_ERROR: u8 = 0x8F;
const HIDPP20_ERROR: u8 = 0xFF;

/// Builds a HID++ frame: 7 bytes for [`SHORT`], 20 for [`LONG`], with
/// `params` zero-padded or truncated to fit.
pub fn request(
    report_id: u8,
    index: u8,
    sub_or_feature: u8,
    fn_or_addr: u8,
    params: &[u8],
) -> Vec<u8> {
    debug_assert!(
        report_id == SHORT || report_id == LONG,
        "not a HID++ report id: {report_id:#04x}"
    );
    let len = if report_id == SHORT {
        SHORT_LEN
    } else {
        LONG_LEN
    };
    let params = &params[..params.len().min(len - HEADER_LEN)];
    let mut frame = Vec::with_capacity(len);
    frame.extend_from_slice(&[report_id, index, sub_or_feature, fn_or_addr]);
    frame.extend_from_slice(params);
    frame.resize(len, 0);
    frame
}

/// Builds a HID++ 2.0 feature request (`function` is 0..=15).
pub fn fap(report_id: u8, index: u8, feature_index: u8, function: u8, params: &[u8]) -> Vec<u8> {
    request(
        report_id,
        index,
        feature_index,
        function_byte(function),
        params,
    )
}

/// How a received frame relates to a request we sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply<'a> {
    /// The answer; borrows the frame from byte 4 on.
    Ok(&'a [u8]),
    /// HID++ 2.0 error code.
    Err20(u8),
    /// HID++ 1.0 error code.
    Err10(u8),
    /// Not an answer to this request.
    NotMine,
}

/// Classifies `frame` against a [`fap`] request with the same arguments.
pub fn match_fap(frame: &[u8], index: u8, feature_index: u8, function: u8) -> Reply<'_> {
    let fn_sw = function_byte(function);
    match *frame {
        [SHORT | LONG, i, f, fs, ..] if i == index && f == feature_index && fs == fn_sw => {
            Reply::Ok(&frame[HEADER_LEN..])
        }
        [SHORT | LONG, i, HIDPP20_ERROR, f, fs, code, ..]
            if i == index && f == feature_index && fs == fn_sw =>
        {
            Reply::Err20(code)
        }
        // HID++ 1.0 devices answer a 2.0 request (e.g. the protocol ping) this way.
        [SHORT | LONG, i, HIDPP10_ERROR, f, fs, code, ..]
            if i == index && f == feature_index && fs == fn_sw =>
        {
            Reply::Err10(code)
        }
        _ => Reply::NotMine,
    }
}

/// Classifies `frame` against a HID++ 1.0 register request.
pub fn match_register(frame: &[u8], index: u8, sub_id: u8, address: u8) -> Reply<'_> {
    match *frame {
        [SHORT | LONG, i, s, a, ..] if i == index && s == sub_id && a == address => {
            Reply::Ok(&frame[HEADER_LEN..])
        }
        [SHORT | LONG, i, HIDPP10_ERROR, s, a, code, ..]
            if i == index && s == sub_id && a == address =>
        {
            Reply::Err10(code)
        }
        _ => Reply::NotMine,
    }
}

fn function_byte(function: u8) -> u8 {
    debug_assert!(function < 0x10, "HID++ function out of range: {function}");
    function << 4 | SW_ID
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long(head: &[u8]) -> Vec<u8> {
        let mut f = head.to_vec();
        f.resize(20, 0);
        f
    }

    #[test]
    fn fap_short_layout() {
        assert_eq!(
            fap(0x10, 0xFF, 0x00, 1, &[0, 0, 0x5A]),
            [0x10, 0xFF, 0x00, 0x1B, 0x00, 0x00, 0x5A]
        );
    }

    #[test]
    fn fap_long_layout_is_20_bytes() {
        let f = fap(0x11, 0x01, 0x05, 1, &[0x00]);
        assert_eq!(f.len(), 20);
        assert_eq!(f[3], 0x1B);
    }

    #[test]
    fn request_short_zero_pads() {
        assert_eq!(
            request(SHORT, 0xFF, 0x81, 0x07, &[0x01]),
            [0x10, 0xFF, 0x81, 0x07, 0x01, 0x00, 0x00]
        );
    }

    #[test]
    fn request_truncates_params_to_fit() {
        assert_eq!(
            request(SHORT, 0x01, 0x80, 0x02, &[1, 2, 3, 4, 5]),
            [0x10, 0x01, 0x80, 0x02, 1, 2, 3]
        );
        let f = request(LONG, 0x01, 0x05, 0x1B, &[0xAA; 30]);
        assert_eq!(f.len(), 20);
        assert_eq!(&f[4..], &[0xAA; 16]);
    }

    #[test]
    fn match_ok() {
        let f = long(&[0x11, 0x01, 0x05, 0x1B, 0x2A]);
        let Reply::Ok(params) = match_fap(&f, 0x01, 0x05, 1) else {
            panic!("expected Ok");
        };
        assert_eq!(params.len(), 16);
        assert_eq!(params[0], 0x2A);
        assert!(params[1..].iter().all(|&b| b == 0));
    }

    #[test]
    fn match_foreign_sw_id_is_not_mine() {
        let f = long(&[0x11, 0x01, 0x05, 0x1D, 0x2A]);
        assert_eq!(match_fap(&f, 0x01, 0x05, 1), Reply::NotMine);
    }

    #[test]
    fn match_other_device_or_feature_is_not_mine() {
        let f = long(&[0x11, 0x01, 0x05, 0x1B, 0x2A]);
        assert_eq!(match_fap(&f, 0x02, 0x05, 1), Reply::NotMine);
        assert_eq!(match_fap(&f, 0x01, 0x06, 1), Reply::NotMine);
        assert_eq!(match_fap(&f, 0x01, 0x05, 2), Reply::NotMine);
    }

    #[test]
    fn match_unknown_report_id_is_not_mine() {
        let f = long(&[0x20, 0x01, 0x05, 0x1B, 0x2A]);
        assert_eq!(match_fap(&f, 0x01, 0x05, 1), Reply::NotMine);
    }

    #[test]
    fn match_err20() {
        let f = long(&[0x11, 0x01, 0xFF, 0x05, 0x1B, 0x05]);
        assert_eq!(match_fap(&f, 0x01, 0x05, 1), Reply::Err20(0x05));
    }

    #[test]
    fn match_err20_for_other_request_is_not_mine() {
        let f = long(&[0x11, 0x01, 0xFF, 0x05, 0x2B, 0x05]);
        assert_eq!(match_fap(&f, 0x01, 0x05, 1), Reply::NotMine);
    }

    #[test]
    fn match_err10_on_ping() {
        let f = [0x10, 0x02, 0x8F, 0x00, 0x1B, 0x09, 0x00];
        assert_eq!(match_fap(&f, 0x02, 0x00, 1), Reply::Err10(0x09));
    }

    #[test]
    fn match_error_for_other_device_is_not_mine() {
        let err20 = long(&[0x11, 0x02, 0xFF, 0x05, 0x1B, 0x05]);
        assert_eq!(match_fap(&err20, 0x01, 0x05, 1), Reply::NotMine);
        let err10 = [0x10, 0x03, 0x8F, 0x00, 0x1B, 0x09, 0x00];
        assert_eq!(match_fap(&err10, 0x02, 0x00, 1), Reply::NotMine);
    }

    #[test]
    fn match_register_non_hidpp_report_is_not_mine() {
        // A DJ report (id 0x20) whose bytes happen to line up.
        let dj = [0x20, 0xFF, 0x81, 0x00, 0x00, 0x01, 0x00];
        assert_eq!(match_register(&dj, 0xFF, 0x81, 0x00), Reply::NotMine);
    }

    #[test]
    fn match_short_frames_are_not_mine() {
        for len in 0..6 {
            let ok = [0x10, 0x01, 0x05, 0x1B, 0x2A, 0x00];
            let err20 = [0x11, 0x01, 0xFF, 0x05, 0x1B, 0x05];
            let err10 = [0x10, 0x01, 0x8F, 0x05, 0x1B, 0x05];
            if len < 4 {
                assert_eq!(match_fap(&ok[..len], 0x01, 0x05, 1), Reply::NotMine);
            }
            assert_eq!(match_fap(&err20[..len], 0x01, 0x05, 1), Reply::NotMine);
            assert_eq!(match_fap(&err10[..len], 0x01, 0x05, 1), Reply::NotMine);
            assert_eq!(
                match_register(&err10[..len], 0x01, 0x05, 0x1B),
                Reply::NotMine
            );
            if len < 4 {
                assert_eq!(match_register(&ok[..len], 0x01, 0x05, 0x1B), Reply::NotMine);
            }
        }
    }

    #[test]
    fn match_register_ok_and_err() {
        let ok = [0x10, 0xFF, 0x81, 0x00, 0x00, 0x01, 0x00];
        assert_eq!(
            match_register(&ok, 0xFF, 0x81, 0x00),
            Reply::Ok(&[0x00, 0x01, 0x00])
        );
        let err = [0x10, 0xFF, 0x8F, 0x81, 0x00, 0x02, 0x00];
        assert_eq!(match_register(&err, 0xFF, 0x81, 0x00), Reply::Err10(0x02));
    }

    #[test]
    fn match_register_for_other_device_is_not_mine() {
        let ok = [0x10, 0xFF, 0x81, 0x00, 0x00, 0x01, 0x00];
        assert_eq!(match_register(&ok, 0x01, 0x81, 0x00), Reply::NotMine);
        assert_eq!(match_register(&ok, 0xFF, 0x81, 0x02), Reply::NotMine);
        let err = [0x10, 0xFF, 0x8F, 0x81, 0x02, 0x02, 0x00];
        assert_eq!(match_register(&err, 0xFF, 0x81, 0x00), Reply::NotMine);
    }
}
