use crate::config::PeerConfig;
use std::io::Read;
use std::time::Duration;

pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub fn build_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(20))
        .timeout_write(Duration::from_secs(20))
        .user_agent(concat!("roke/", env!("CARGO_PKG_VERSION")))
        .build()
}

pub fn build_no_redirect_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(20))
        .timeout_write(Duration::from_secs(20))
        .user_agent(concat!("roke/", env!("CARGO_PKG_VERSION")))
        .build()
}

pub fn basic_auth_header(peer: &PeerConfig) -> Result<Option<String>, String> {
    let user = peer.username.as_deref().unwrap_or("").trim();
    let pass = peer.resolve_password()?;
    if user.is_empty() && pass.is_none() {
        return Ok(None);
    }
    let creds = format!("{user}:{}", pass.unwrap_or_default());
    Ok(Some(format!("Basic {}", base64_encode(creds.as_bytes()))))
}

pub fn apply_auth(req: ureq::Request, auth: Option<&str>) -> ureq::Request {
    if let Some(header) = auth {
        req.set("Authorization", header)
    } else {
        req
    }
}

pub fn read_response_string(resp: ureq::Response) -> Result<String, String> {
    let mut buf = String::new();
    resp.into_reader()
        .take(32 * 1024 * 1024)
        .read_to_string(&mut buf)
        .map_err(|e| format!("failed reading HTTP body: {e}"))?;
    Ok(buf)
}

pub fn normalize_etag(raw: &str) -> String {
    raw.trim().to_string()
}

/// Resolve an absolute or relative `href` (from a DAV multistatus response) against `base_url`.
pub fn resolve_href(base_url: &str, href: &str) -> String {
    let href = href.trim();
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    if let Some(scheme_end) = base_url.find("://") {
        let after_scheme = &base_url[scheme_end + 3..];
        let origin_len = after_scheme
            .find('/')
            .map(|i| scheme_end + 3 + i)
            .unwrap_or(base_url.len());
        let origin = &base_url[..origin_len];
        if href.starts_with('/') {
            return format!("{origin}{href}");
        }
        let base_dir = if base_url.ends_with('/') {
            base_url.to_string()
        } else if let Some(last_slash) = base_url[origin_len..].rfind('/') {
            format!(
                "{}{}",
                origin,
                &base_url[origin_len..=origin_len + last_slash]
            )
        } else {
            format!("{origin}/")
        };
        return format!("{base_dir}{href}");
    }
    href.to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavResponse {
    pub href: String,
    pub etag: Option<String>,
    pub calendar_data: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DavDiscoveryEntry {
    pub href: String,
    pub displayname: Option<String>,
    pub is_calendar: bool,
    /// `Some(true)` if `VTODO` is advertised, `Some(false)` if a component set is present without
    /// `VTODO` (e.g. events-only `VEVENT`), or `None` if unspecified (all components allowed).
    pub supports_vtodo: Option<bool>,
    pub current_user_principal: Option<String>,
    pub calendar_home_set: Option<String>,
}

/// Extract origin (`scheme://host[:port]`) from an HTTP(S) URL.
pub fn url_origin(url: &str) -> Option<&str> {
    let scheme_end = url.find("://")?;
    let after_scheme = &url[scheme_end + 3..];
    let origin_len = after_scheme
        .find('/')
        .map(|i| scheme_end + 3 + i)
        .unwrap_or(url.len());
    Some(&url[..origin_len])
}

/// Parse a WebDAV/CalDAV `PROPFIND` `multistatus` response for discovery metadata
/// (`current-user-principal`, `calendar-home-set`, `resourcetype`, `displayname`, and VTODO support).
pub fn parse_discovery_multistatus(xml: &str) -> Vec<DavDiscoveryEntry> {
    let mut results = Vec::new();
    for block in extract_elements(xml, "response") {
        let Some(href_raw) = extract_first_element(block, "href") else {
            continue;
        };
        let href = unescape_xml(href_raw.trim());
        let displayname = extract_first_element(block, "displayname")
            .map(|s| unescape_xml(s.trim()))
            .filter(|s| !s.is_empty());
        let is_calendar = extract_first_element(block, "resourcetype")
            .is_some_and(|rt| !extract_elements(rt, "calendar").is_empty());
        let supports_vtodo = extract_first_element(block, "supported-calendar-component-set")
            .map(|set| set.to_ascii_uppercase().contains("VTODO"));
        let current_user_principal = extract_first_element(block, "current-user-principal")
            .and_then(|p| extract_first_element(p, "href"))
            .map(|h| unescape_xml(h.trim()))
            .filter(|h| !h.is_empty());
        let calendar_home_set = extract_first_element(block, "calendar-home-set")
            .and_then(|c| extract_first_element(c, "href"))
            .map(|h| unescape_xml(h.trim()))
            .filter(|h| !h.is_empty());

        results.push(DavDiscoveryEntry {
            href,
            displayname,
            is_calendar,
            supports_vtodo,
            current_user_principal,
            calendar_home_set,
        });
    }
    results
}

/// Parse a WebDAV/CalDAV XML `multistatus` document into `DavResponse` entries.
/// Namespace-agnostic: matches `<D:response>`, `<d:response>`, `<response>`, etc.
pub fn parse_multistatus(xml: &str) -> Vec<DavResponse> {
    let mut results = Vec::new();
    for block in extract_elements(xml, "response") {
        let Some(href_raw) = extract_first_element(block, "href") else {
            continue;
        };
        let href = unescape_xml(href_raw.trim());
        let etag = extract_first_element(block, "getetag")
            .map(|s| normalize_etag(&unescape_xml(s.trim())))
            .filter(|s| !s.is_empty());
        let calendar_data = extract_first_element(block, "calendar-data").map(unescape_xml);
        let status = extract_first_element(block, "status").map(|s| unescape_xml(s.trim()));
        results.push(DavResponse {
            href,
            etag,
            calendar_data,
            status,
        });
    }
    results
}

fn tag_local_name(tag_token: &str) -> &str {
    let name_part = tag_token
        .split(|c: char| c.is_whitespace() || c == '/')
        .next()
        .unwrap_or(tag_token);
    name_part.rsplit(':').next().unwrap_or(name_part)
}

fn extract_first_element<'a>(xml: &'a str, local_name: &str) -> Option<&'a str> {
    extract_elements(xml, local_name).into_iter().next()
}

/// Extract inner XML/text for every top-level occurrence of elements whose local name matches
/// `local_name` (case-insensitive). Skips XML comments and CDATA sections while scanning tags.
fn extract_elements<'a>(xml: &'a str, local_name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let bytes = xml.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let Some(lt_rel) = xml[i..].find('<') else {
            break;
        };
        let lt = i + lt_rel;
        if xml[lt..].starts_with("<!--") {
            if let Some(end_rel) = xml[lt + 4..].find("-->") {
                i = lt + 4 + end_rel + 3;
                continue;
            } else {
                break;
            }
        }
        if xml[lt..].starts_with("<![CDATA[") {
            if let Some(end_rel) = xml[lt + 9..].find("]]>") {
                i = lt + 9 + end_rel + 3;
                continue;
            } else {
                break;
            }
        }
        let Some(gt_rel) = xml[lt + 1..].find('>') else {
            break;
        };
        let gt = lt + 1 + gt_rel;
        let tag_inner = xml[lt + 1..gt].trim();

        if tag_inner.starts_with('/') || tag_inner.starts_with('?') || tag_inner.starts_with('!') {
            i = gt + 1;
            continue;
        }
        let self_closing = tag_inner.ends_with('/');
        if !tag_local_name(tag_inner).eq_ignore_ascii_case(local_name) {
            i = gt + 1;
            continue;
        }
        if self_closing {
            out.push("");
            i = gt + 1;
            continue;
        }

        // Scan forward to matching closing tag, tracking nested same-name elements
        let content_start = gt + 1;
        let mut depth = 1usize;
        let mut cursor = content_start;
        let mut found_end: Option<(usize, usize)> = None;

        while cursor < bytes.len() {
            let Some(next_lt_rel) = xml[cursor..].find('<') else {
                break;
            };
            let next_lt = cursor + next_lt_rel;
            if xml[next_lt..].starts_with("<!--") {
                if let Some(end_rel) = xml[next_lt + 4..].find("-->") {
                    cursor = next_lt + 4 + end_rel + 3;
                    continue;
                } else {
                    break;
                }
            }
            if xml[next_lt..].starts_with("<![CDATA[") {
                if let Some(end_rel) = xml[next_lt + 9..].find("]]>") {
                    cursor = next_lt + 9 + end_rel + 3;
                    continue;
                } else {
                    break;
                }
            }
            let Some(next_gt_rel) = xml[next_lt + 1..].find('>') else {
                break;
            };
            let next_gt = next_lt + 1 + next_gt_rel;
            let inner = xml[next_lt + 1..next_gt].trim();
            if let Some(closing) = inner.strip_prefix('/') {
                if tag_local_name(closing.trim()).eq_ignore_ascii_case(local_name) {
                    depth -= 1;
                    if depth == 0 {
                        found_end = Some((next_lt, next_gt + 1));
                        break;
                    }
                }
            } else if !inner.starts_with('?')
                && !inner.starts_with('!')
                && !inner.ends_with('/')
                && tag_local_name(inner).eq_ignore_ascii_case(local_name)
            {
                depth += 1;
            }
            cursor = next_gt + 1;
        }

        if let Some((content_end, after_close)) = found_end {
            out.push(&xml[content_start..content_end]);
            i = after_close;
        } else {
            i = gt + 1;
        }
    }

    out
}

pub fn unescape_xml(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;

    while !rest.is_empty() {
        if let Some(after_cdata) = rest.strip_prefix("<![CDATA[") {
            if let Some(end) = after_cdata.find("]]>") {
                out.push_str(&after_cdata[..end]);
                rest = &after_cdata[end + 3..];
                continue;
            }
        }
        if let Some(after_amp) = rest.strip_prefix('&') {
            if let Some(semi) = after_amp.find(';') {
                let entity = &after_amp[..semi];
                let decoded: Option<char> = match entity {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                        u32::from_str_radix(&entity[2..], 16)
                            .ok()
                            .and_then(char::from_u32)
                    }
                    _ if entity.starts_with('#') => {
                        entity[1..].parse::<u32>().ok().and_then(char::from_u32)
                    }
                    _ => None,
                };
                if let Some(ch) = decoded {
                    out.push(ch);
                    rest = &after_amp[semi + 1..];
                    continue;
                }
            }
        }
        let mut chars = rest.chars();
        if let Some(ch) = chars.next() {
            out.push(ch);
            rest = chars.as_str();
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_href_handles_absolute_root_and_relative_paths() {
        assert_eq!(
            resolve_href("https://dav.example.com/user/tasks/", "/user/tasks/a.ics"),
            "https://dav.example.com/user/tasks/a.ics"
        );
        assert_eq!(
            resolve_href("https://dav.example.com:5232/user/tasks/", "a.ics"),
            "https://dav.example.com:5232/user/tasks/a.ics"
        );
        assert_eq!(
            resolve_href("https://dav.example.com/user/tasks", "a.ics"),
            "https://dav.example.com/user/a.ics"
        );
        assert_eq!(
            resolve_href(
                "https://dav.example.com/user/tasks/",
                "https://other.example.com/x.ics"
            ),
            "https://other.example.com/x.ics"
        );
    }

    #[test]
    fn parse_multistatus_handles_namespaces_entities_and_cdata() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response>
    <D:href>/user/tasks/one.ics</D:href>
    <D:propstat>
      <D:prop>
        <D:getetag>&quot;abc-123&quot;</D:getetag>
        <C:calendar-data>BEGIN:VCALENDAR
SUMMARY:Buy &amp; cook &lt;pasta&gt;
END:VCALENDAR</C:calendar-data>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <response xmlns="DAV:">
    <href>/user/tasks/two.ics</href>
    <propstat>
      <prop>
        <getetag>"def-456"</getetag>
        <cal:calendar-data xmlns:cal="urn:ietf:params:xml:ns:caldav"><![CDATA[BEGIN:VCALENDAR
SUMMARY:Raw & unescaped
END:VCALENDAR]]></cal:calendar-data>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</D:multistatus>"#;

        let items = parse_multistatus(xml);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].href, "/user/tasks/one.ics");
        assert_eq!(items[0].etag.as_deref(), Some("\"abc-123\""));
        assert!(items[0]
            .calendar_data
            .as_deref()
            .unwrap()
            .contains("SUMMARY:Buy & cook <pasta>"));

        assert_eq!(items[1].href, "/user/tasks/two.ics");
        assert_eq!(items[1].etag.as_deref(), Some("\"def-456\""));
        assert!(items[1]
            .calendar_data
            .as_deref()
            .unwrap()
            .contains("SUMMARY:Raw & unescaped"));
    }
}
