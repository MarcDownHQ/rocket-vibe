//! Teams' SDP dialect against plain WebRTC's (brain/architecture/native-calls.md):
//! the offers and answers the calling stack carries, and what libwebrtc reads.

/// A plain WebRTC offer, audio only, in Teams' dialect: its profile is written
/// `RTP/SAVP`, its extension URIs with backslashes, and the audio section carries
/// its label and the range of the source it sends. The media line names a real
/// address, taken from a candidate: port 9 and `0.0.0.0` read as disabled media
/// to Teams (`DisabledMediaNotSupported`). UDP candidates only, without the
/// extras Teams does not parse.
pub fn teams_offer(sdp: &str) -> String {
    let ssrc = sdp.lines().find_map(|l| l.strip_prefix("a=ssrc:")?.split(' ').next()).map(str::to_owned);
    let default = sdp.lines().filter_map(candidate_fields).find(|c| c.udp && !c.address.contains(':'));
    let port = default.as_ref().map_or("9", |c| c.port);
    let address = default.as_ref().map_or("0.0.0.0", |c| c.address);
    let mut out = String::with_capacity(sdp.len() + 128);
    let mut push = |line: &str| {
        out.push_str(line);
        out.push_str("\r\n");
    };
    for line in sdp.lines() {
        if let Some(rest) = line.strip_prefix("m=audio ") {
            let (_, rest) = rest.split_once(' ').unwrap_or(("", rest));
            push(&format!("m=audio {port} {}", rest.replacen("UDP/TLS/RTP/SAVPF", "RTP/SAVP", 1)));
        } else if line.starts_with("c=") {
            push(&format!("c=IN IP4 {address}"));
        } else if line.starts_with("a=rtcp:") {
            push(&format!("a=rtcp:{port}"));
        } else if line.starts_with("a=extmap:") {
            push(&line.replace('/', "\\"));
        } else if line == "a=rtcp-rsize" {
        } else if line.starts_with("a=candidate:") {
            let Some(c) = candidate_fields(line).filter(|c| c.udp) else { continue };
            push(&format!("a=candidate:{}", c.plain));
        } else if line.starts_with("a=mid:") {
            push(line);
            push("a=label:main-audio");
            push("a=x-signaling-fb:* x-message app recv:dsh");
            if let Some(ssrc) = &ssrc {
                push(&format!("a=x-ssrc-range:{ssrc}-{ssrc}"));
            }
        } else {
            push(line);
        }
    }
    out
}

struct OfferCandidate<'a> {
    udp: bool,
    address: &'a str,
    port: &'a str,
    /// Up to its type and related address, without `generation`, `ufrag`, `network-id`.
    plain: String,
}

fn candidate_fields(line: &str) -> Option<OfferCandidate<'_>> {
    let fields: Vec<&str> = line.strip_prefix("a=candidate:")?.split(' ').collect();
    let end = fields.iter().position(|f| *f == "generation").unwrap_or(fields.len());
    Some(OfferCandidate {
        udp: fields.get(2)?.eq_ignore_ascii_case("udp"),
        address: fields.get(4)?,
        port: fields.get(5)?,
        plain: fields[..end].join(" "),
    })
}

/// Teams' answer in plain WebRTC: only its audio section, with the profile and
/// extension URIs libwebrtc reads, and the UDP candidates without their extras.
pub fn webrtc_answer(sdp: &str) -> Option<String> {
    #[derive(PartialEq)]
    enum In {
        Head,
        Audio,
        Other,
    }
    let mut head = Vec::new();
    let mut audio: Option<Vec<String>> = None;
    let mut at = In::Head;
    for line in sdp.lines() {
        if line.starts_with("m=") {
            at = if line.starts_with("m=audio ") && audio.is_none() {
                audio = Some(vec![line.replacen("RTP/SAVP", "UDP/TLS/RTP/SAVPF", 1)]);
                In::Audio
            } else {
                In::Other
            };
            continue;
        }
        if at == In::Head {
            if !line.starts_with("a=group:") && !line.starts_with("a=x-") {
                head.push(line.to_owned());
            }
            continue;
        }
        if at == In::Other {
            continue;
        }
        let Some(lines) = audio.as_mut() else { continue };
        if line.starts_with("a=x-") || line.starts_with("a=label:") || line.starts_with("a=ptime:") {
            continue;
        }
        if let Some(candidate) = line.strip_prefix("a=candidate:") {
            let fields: Vec<&str> = candidate.split(' ').collect();
            if fields.get(2).is_none_or(|p| !p.eq_ignore_ascii_case("udp")) {
                continue;
            }
            let end = fields.iter().position(|f| *f == "MTURNID").unwrap_or(fields.len());
            lines.push(format!("a=candidate:{}", fields[..end].join(" ")));
            continue;
        }
        if line.starts_with("a=extmap:") {
            lines.push(line.replace('\\', "/"));
            continue;
        }
        lines.push(line.to_owned());
    }
    let audio = audio?;
    let mid = audio.iter().find_map(|l| l.strip_prefix("a=mid:"))?.to_owned();
    let mut out = String::new();
    for line in head {
        out.push_str(&line);
        out.push_str("\r\n");
        if line.starts_with("t=") {
            out.push_str(&format!("a=group:BUNDLE {mid}\r\n"));
        }
    }
    for line in audio {
        out.push_str(&line);
        out.push_str("\r\n");
    }
    Some(out)
}

/// The first audio section's `a=mid`.
pub fn audio_mid(sdp: &str) -> Option<&str> {
    sdp.lines().skip_while(|l| !l.starts_with("m=audio ")).find_map(|l| l.strip_prefix("a=mid:"))
}

fn with_mid(sdp: &str, mid: &str) -> String {
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.lines() {
        if line.starts_with("a=mid:") {
            out.push_str(&format!("a=mid:{mid}\r\n"));
        } else if line.starts_with("a=group:BUNDLE") {
            out.push_str(&format!("a=group:BUNDLE {mid}\r\n"));
        } else {
            out.push_str(line);
            out.push_str("\r\n");
        }
    }
    out
}

/// A new offer of Teams' in plain WebRTC, made to fit what `negotiated` (the call's
/// current plain SDP) set: libwebrtc refuses a later offer whose section has another
/// `mid`, or that gives an extension id another URI. Teams' hold offer moves id 3 from
/// transport-wide-cc to its own feedback extension: such an extension is left out, so
/// the answer declines it.
pub fn renegotiation_offer(sdp: &str, negotiated: Option<&str>) -> Option<String> {
    let plain = webrtc_answer(sdp)?;
    let Some(negotiated) = negotiated else { return Some(plain) };
    let active = extensions(negotiated);
    let mut kept = String::with_capacity(plain.len());
    for line in plain.lines() {
        let clashes = extension(line).is_some_and(|(id, uri)| {
            active.iter().any(|&(other_id, other_uri)| (other_id == id) != (other_uri == uri))
        });
        if !clashes {
            kept.push_str(line);
            kept.push_str("\r\n");
        }
    }
    Some(audio_mid(negotiated).map_or_else(|| kept.clone(), |mid| with_mid(&kept, mid)))
}

/// An `a=extmap` line's id and URI.
fn extension(line: &str) -> Option<(&str, &str)> {
    let mut fields = line.strip_prefix("a=extmap:")?.split(' ');
    let id = fields.next()?.split('/').next()?;
    Some((id, fields.next()?))
}

fn extensions(sdp: &str) -> Vec<(&str, &str)> {
    sdp.lines().filter_map(extension).collect()
}

/// `RV_CALL_DEBUG=1`: each section of an SDP: its kind, `mid` and direction.
pub fn sdp_sections(sdp: &str) -> String {
    let mut out = Vec::new();
    for line in sdp.lines() {
        if let Some(rest) = line.strip_prefix("m=") {
            let mut fields = rest.split(' ');
            out.push(format!("{} port {}", fields.next().unwrap_or(""), fields.next().unwrap_or("")));
        } else if let (Some(last), Some(mid)) = (out.last_mut(), line.strip_prefix("a=mid:")) {
            let last: &mut String = last;
            last.push_str(&format!(" mid {mid}"));
        } else if let Some(last) = out.last_mut()
            && matches!(line, "a=sendrecv" | "a=sendonly" | "a=recvonly" | "a=inactive")
        {
            last.push_str(&format!(" {}", &line[2..]));
        }
    }
    out.join("; ")
}

/// A local WebRTC answer to Teams' `offer`, in Teams' dialect: the same sections in
/// the same order, each but the audio one refused with port 0, as its own client does.
pub fn teams_answer(answer: &str, offer: &str) -> String {
    let answer = audio_mid(offer).map_or_else(|| answer.to_owned(), |mid| with_mid(answer, mid));
    let audio = teams_offer(&answer);
    let mut sections: Vec<Vec<&str>> = vec![Vec::new()];
    for line in offer.lines() {
        if line.starts_with("m=") {
            sections.push(Vec::new());
        }
        sections.last_mut().expect("never empty").push(line);
    }
    let mut out = String::with_capacity(audio.len() + 128);
    let (head, body) = audio.split_once("m=audio ").map_or((audio.as_str(), ""), |(h, b)| (h, b));
    out.push_str(head);
    let mut placed = false;
    for section in sections.iter().skip(1) {
        let line = section[0];
        if line.starts_with("m=audio ") && !placed {
            placed = true;
            out.push_str("m=audio ");
            out.push_str(body);
            continue;
        }
        let mut fields = line.splitn(3, ' ');
        let (kind, _port, rest) = (fields.next().unwrap_or(""), fields.next(), fields.next().unwrap_or(""));
        out.push_str(&format!("{kind} 0 {rest}\r\n"));
        if let Some(mid) = section.iter().find(|l| l.starts_with("a=mid:")) {
            out.push_str(mid);
            out.push_str("\r\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\n\
        m=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\n\
        a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n\
        a=rtcp:9 IN IP4 0.0.0.0\r\na=rtcp-rsize\r\na=mid:0\r\na=rtpmap:111 opus/48000/2\r\na=ssrc:42 cname:x\r\n\
        a=candidate:1 1 udp 21 2001:db8::1 111 typ host generation 0 ufrag u network-id 1\r\n\
        a=candidate:2 1 udp 22 192.168.1.5 222 typ host generation 0 ufrag u network-id 1\r\n\
        a=candidate:3 1 tcp 23 192.168.1.5 333 typ host tcptype passive generation 0\r\n";

    const ANSWER: &str = "v=0\r\no=- 455788 0 IN IP4 127.0.0.1\r\ns=session\r\nc=IN IP4 52.1.2.3\r\nb=CT:10000000\r\nt=0 0\r\n\
        a=group:BUNDLE 0 3\r\na=x-plaza-msi-range:1-100 101-200\r\n\
        m=audio 3480 RTP/SAVP 120 111\r\nc=IN IP4 52.1.2.3\r\na=x-signaling-fb:* x-message app send:dsh\r\na=rtcp:3480\r\n\
        a=setup:passive\r\na=ice-ufrag:u\r\na=ice-pwd:p\r\na=rtcp-mux\r\n\
        a=candidate:1 1 UDP 54001663 52.1.2.3 3480 typ relay raddr 10.0.0.1 rport 3480 MTURNID 154\r\n\
        a=candidate:3 1 tcp-pass 18087935 52.1.2.3 3478 typ relay raddr 10.0.0.1 rport 3478\r\n\
        a=label:main-audio\r\na=mid:0\r\na=rtpmap:120 CN/48000\r\na=rtpmap:111 OPUS/48000/2\r\na=ptime:20\r\n\
        a=fingerprint:sha-256 AA\r\na=extmap:2 http:\\\\www.webrtc.org\\experiments\\rtp-hdrext\\abs-send-time\r\na=x-ssrc-range:1000-1000\r\n\
        m=video 0 RTP/SAVP 102\r\n\
        m=x-data 3480 RTP/SAVP 127\r\na=mid:3\r\na=label:data\r\n";

    #[test]
    fn a_new_offer_keeps_the_calls_mid_and_its_answer_the_offers() {
        let offer = "v=0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 7 8\r\nm=audio 5 RTP/SAVP 111\r\nc=IN IP4 1.2.3.4\r\n\
            a=inactive\r\na=mid:7\r\nm=video 5 RTP/SAVP 34\r\na=mid:8\r\n";
        let negotiated = "v=0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n";
        let plain = renegotiation_offer(offer, Some(negotiated)).unwrap();
        assert_eq!(audio_mid(&plain), Some("0"));
        assert!(plain.contains("a=group:BUNDLE 0\r\n") && plain.contains("a=inactive") && !plain.contains("m=video"));
        let local = "v=0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\n\
            a=inactive\r\na=mid:0\r\n";
        let answer = teams_answer(local, offer);
        assert_eq!(audio_mid(&answer), Some("7"));
        assert!(answer.contains("a=group:BUNDLE 7\r\n") && answer.contains("m=video 0 ") && answer.contains("a=mid:8"));
        assert_eq!(sdp_sections(offer), "audio port 5 inactive mid 7; video port 5 mid 8");
    }

    #[test]
    fn a_new_offer_leaves_out_an_extension_id_the_call_gave_another_uri() {
        let negotiated = "v=0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n\
            a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n\
            a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n";
        let offer = "v=0\r\ns=-\r\nt=0 0\r\nm=audio 5 RTP/SAVP 111\r\nc=IN IP4 1.2.3.4\r\na=mid:1\r\n\
            a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n\
            a=extmap:3 http:\\\\skype.com\\experiments\\rtp-hdrext\\fast_bandwidth_feedback#version_3\r\n\
            a=extmap:5 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n\
            a=extmap:6 urn:ietf:params:rtp-hdrext:sdes:mid\r\n";
        let plain = renegotiation_offer(offer, Some(negotiated)).unwrap();
        assert_eq!(audio_mid(&plain), Some("0"));
        assert!(plain.contains("a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n"));
        assert!(plain.contains("a=extmap:6 urn:ietf:params:rtp-hdrext:sdes:mid\r\n"));
        assert!(!plain.contains("a=extmap:3 ") && !plain.contains("a=extmap:5 "), "{plain}");
        assert_eq!(renegotiation_offer(offer, None).unwrap().matches("a=extmap:").count(), 4);
    }

    #[test]
    fn an_offer_speaks_teams_dialect() {
        let offer = teams_offer(OFFER);
        assert!(offer.contains("m=audio 222 RTP/SAVP 111\r\n"), "{offer}");
        assert!(offer.contains("c=IN IP4 192.168.1.5\r\n"), "{offer}");
        assert!(offer.contains("a=rtcp:222\r\n"), "{offer}");
        assert!(offer.contains("a=candidate:2 1 udp 22 192.168.1.5 222 typ host\r\n"), "{offer}");
        assert!(offer.contains("a=candidate:1 1 udp 21 2001:db8::1 111 typ host\r\n"), "{offer}");
        assert!(!offer.contains("tcptype"), "{offer}");
        assert!(!offer.contains("rtcp-rsize"), "{offer}");
        assert!(
            offer.contains("a=extmap:2 http:\\\\www.webrtc.org\\experiments\\rtp-hdrext\\abs-send-time\r\n"),
            "{offer}"
        );
        assert!(offer.contains("a=label:main-audio\r\n"));
        assert!(offer.contains("a=x-ssrc-range:42-42\r\n"));
    }

    #[test]
    fn an_answer_keeps_only_plain_webrtc_audio() {
        let answer = webrtc_answer(ANSWER).unwrap();
        assert!(answer.contains("a=group:BUNDLE 0\r\n"), "{answer}");
        assert!(answer.contains("m=audio 3480 UDP/TLS/RTP/SAVPF 120 111\r\n"));
        assert!(
            answer.contains("a=candidate:1 1 UDP 54001663 52.1.2.3 3480 typ relay raddr 10.0.0.1 rport 3480\r\n"),
            "{answer}"
        );
        assert!(!answer.contains("MTURNID"));
        assert!(!answer.contains("tcp-pass"));
        assert!(!answer.contains("m=video"));
        assert!(!answer.contains("x-data"));
        assert!(!answer.contains("a=x-"));
        assert!(!answer.contains("a=label"));
        assert!(
            answer.contains("a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n"),
            "{answer}"
        );
        assert_eq!(webrtc_answer("v=0\r\nm=video 0 RTP/SAVP 1\r\n"), None);
    }

    #[test]
    fn an_offer_from_teams_is_answered_in_its_order() {
        let offer = "v=0\r\nt=0 0\r\nm=audio 3480 RTP/SAVP 120 111\r\nc=IN IP4 52.1.2.3\r\na=mid:0\r\n\
            m=video 0 RTP/SAVP 102\r\na=mid:1\r\nm=x-data 3480 RTP/SAVP 127\r\na=mid:3\r\n";
        assert!(webrtc_answer(offer).unwrap().contains("m=audio 3480 UDP/TLS/RTP/SAVPF 120 111\r\n"));
        let local = "v=0\r\nt=0 0\r\na=group:BUNDLE 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP4 0.0.0.0\r\n\
            a=mid:0\r\na=sendonly\r\na=candidate:2 1 udp 22 192.168.1.5 222 typ host generation 0\r\n";
        let answer = teams_answer(local, offer);
        assert!(answer.contains("m=audio 222 RTP/SAVP 111\r\n"), "{answer}");
        assert!(answer.contains("a=sendonly\r\n"));
        assert!(answer.contains("m=video 0 RTP/SAVP 102\r\na=mid:1\r\n"), "{answer}");
        assert!(answer.contains("m=x-data 0 RTP/SAVP 127\r\na=mid:3\r\n"), "{answer}");
        assert!(answer.find("m=audio").unwrap() < answer.find("m=video").unwrap());
    }
}
