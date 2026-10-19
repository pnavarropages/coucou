// WhatsApp in the island: what Coucou keeps of the conversations — in memory only — and the
// rules that decide what reaches the pill. Port of NotchBuddy's WhatsAppModels.swift. Pure data
// and logic, no I/O, so the rules are tested without the app. Nothing here is ever written to
// disk or to a log.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use serde::Serialize;

/// What a message holds besides text: only a label is shown, the media itself is never fetched.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Media {
    None,
    Image,
    Video,
    Voice { seconds: Option<u32> },
    Document { name: Option<String> },
    Sticker,
    Location,
    Contact,
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    /// The conversation: a person (…@s.whatsapp.net, …@lid, …@c.us) or a group (…@g.us).
    pub chat: String,
    /// Who wrote it (in a group, the member; in a private chat, the chat itself).
    pub sender: String,
    pub sender_name: String,
    pub text: String,
    pub media: Media,
    /// Unix seconds.
    pub date: i64,
    pub from_me: bool,
    /// The message mentions this account (only meaningful in groups).
    pub mentions_me: bool,
    /// Set by the gateway when the chat name came with the message.
    pub chat_name: Option<String>,
}

impl Message {
    pub fn new(id: &str, chat: &str, sender: &str, sender_name: &str, text: &str, date: i64) -> Self {
        Message {
            id: id.into(),
            chat: chat.into(),
            sender: sender.into(),
            sender_name: sender_name.into(),
            text: text.into(),
            media: Media::None,
            date,
            from_me: false,
            mentions_me: false,
            chat_name: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    /// The chat JID.
    pub id: String,
    pub name: String,
    pub is_group: bool,
    /// Oldest first, the last `Inbox::KEEP` only.
    pub messages: Vec<Message>,
    pub unread: u32,
    pub last_activity: i64,
}

/// What the user chose in Settings → WhatsApp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filters {
    /// Groups stay quiet unless the message mentions you.
    pub mute_groups: bool,
    /// Chats never shown (normalised JIDs).
    pub ignored: BTreeSet<String>,
}

impl Default for Filters {
    fn default() -> Self {
        Filters { mute_groups: true, ignored: BTreeSet::new() }
    }
}

// ── JIDs ─────────────────────────────────────────────────────────────────────

pub fn is_group(jid: &str) -> bool {
    jid.ends_with("@g.us")
}

/// "33612345678@s.whatsapp.net" → "+33612345678".
pub fn display_number(jid: &str) -> String {
    let user = jid.split('@').next().unwrap_or(jid);
    let digits = user.split(':').next().unwrap_or(user);
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        format!("+{digits}")
    } else {
        digits.to_string()
    }
}

/// The same person under its several forms: "33612345678:12@s.whatsapp.net" and
/// "33612345678@c.us" are one chat.
pub fn normalized(jid: &str) -> String {
    let Some((user, server)) = jid.split_once('@') else { return jid.to_string() };
    let user = user.split(':').next().unwrap_or(user);
    let server = if server == "c.us" { "s.whatsapp.net" } else { server };
    format!("{user}@{server}")
}

/// The user part's digits: "33612345678:3@s.whatsapp.net" → "33612345678".
pub fn digits(jid: &str) -> String {
    let user = jid.split('@').next().unwrap_or(jid);
    let base = user.split(':').next().unwrap_or(user);
    base.chars().filter(char::is_ascii_digit).collect()
}

/// "@33612345678" in the text, for this account's number (how GOWA shows a mention of you).
pub fn text_mentions(text: &str, me: Option<&str>) -> bool {
    let Some(me) = me else { return false };
    let d = digits(me);
    d.len() >= 6 && text.contains(&format!("@{d}"))
}

// ── Dates ────────────────────────────────────────────────────────────────────

/// Days since 1970-01-01 of a civil date (proleptic Gregorian). Howard Hinnant's algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// RFC 3339 ("2023-10-15T10:30:00Z", fractions and "+07:00" offsets accepted) → Unix seconds.
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let b = text.trim().as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = b.get(from..from + len)?;
        if !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(part).ok()?.parse().ok()
    };
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (hour, minute, second) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    let offset = match b.get(i)? {
        b'Z' | b'z' if i + 1 == b.len() => 0,
        sign @ (b'+' | b'-') => {
            let (h, m) = (num(i + 1, 2)?, num(i + 4, 2)?);
            if b.get(i + 3) != Some(&b':') || i + 6 != b.len() {
                return None;
            }
            let secs = h * 3600 + m * 60;
            if *sign == b'+' { secs } else { -secs }
        }
        _ => return None,
    };
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// Unix seconds → "2023-10-15T10:30:00Z".
pub fn iso(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── The inbox ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Duplicate,
    Ignored,
    /// Kept, but silent (a muted group, your own message, history).
    Quiet,
    /// Should reach you: the pill lights up.
    Notify,
}

/// The conversations, grouped by chat: one entry per conversation, never one per message.
#[derive(Clone, Debug, Default)]
pub struct Inbox {
    conversations: HashMap<String, Conversation>,
    seen: HashSet<String>,
    /// The same ids in arrival order, so the oldest are the ones forgotten.
    seen_order: VecDeque<String>,
}

impl Inbox {
    pub const KEEP: usize = 20;

    pub fn conversation(&self, chat: &str) -> Option<&Conversation> {
        self.conversations.get(&normalized(chat))
    }

    /// Newest activity first.
    pub fn sorted(&self) -> Vec<&Conversation> {
        let mut all: Vec<&Conversation> = self.conversations.values().collect();
        all.sort_by(|a, b| b.last_activity.cmp(&a.last_activity).then_with(|| a.id.cmp(&b.id)));
        all
    }

    /// Adds a message to its conversation. `history`: an older message loaded on demand: kept,
    /// never counted as unread, never announced.
    pub fn add(&mut self, message: Message, filters: &Filters, name: Option<&str>, history: bool) -> Outcome {
        if self.seen.contains(&message.id) {
            return Outcome::Duplicate;
        }
        let chat = normalized(&message.chat);
        if filters.ignored.contains(&chat) {
            return Outcome::Ignored;
        }
        self.seen.insert(message.id.clone());
        self.seen_order.push_back(message.id.clone());
        if self.seen_order.len() > 2_000 {
            for _ in 0..1_000 {
                if let Some(old) = self.seen_order.pop_front() {
                    self.seen.remove(&old);
                }
            }
        }

        let group = is_group(&chat);
        let number = display_number(&chat);
        // In a private chat the other person names it: never your own name on your own message.
        let person_name = if group || message.from_me { None } else { Some(message.sender_name.as_str()).filter(|n| !n.is_empty()) };
        let given = name.or(message.chat_name.as_deref()).filter(|n| !n.is_empty());
        let conversation = self.conversations.entry(chat.clone()).or_insert_with(|| Conversation {
            id: chat.clone(),
            name: given.or(person_name).map(str::to_string).unwrap_or_else(|| number.clone()),
            is_group: group,
            messages: Vec::new(),
            unread: 0,
            last_activity: message.date,
        });
        if let Some(n) = given {
            conversation.name = n.to_string();
        } else if conversation.name == number {
            if let Some(n) = person_name {
                conversation.name = n.to_string();
            }
        }
        let date = message.date;
        let from_me = message.from_me;
        let mentions_me = message.mentions_me;
        let mut stored = message;
        stored.chat = chat;
        conversation.messages.push(stored);
        conversation.messages.sort_by_key(|m| m.date);
        if conversation.messages.len() > Self::KEEP {
            let extra = conversation.messages.len() - Self::KEEP;
            conversation.messages.drain(..extra);
        }
        conversation.last_activity = conversation.last_activity.max(date);

        if history {
            Outcome::Quiet
        } else if from_me {
            // You answered from the phone: it is read there.
            conversation.unread = 0;
            Outcome::Quiet
        } else {
            conversation.unread += 1;
            if group && filters.mute_groups && !mentions_me { Outcome::Quiet } else { Outcome::Notify }
        }
    }

    /// The conversations the pill lists: muted groups without unread mentions stay out of it.
    pub fn visible(&self, filters: &Filters) -> Vec<&Conversation> {
        self.sorted()
            .into_iter()
            .filter(|c| {
                !filters.ignored.contains(&c.id)
                    && (!c.is_group || !filters.mute_groups || c.messages.iter().any(|m| m.mentions_me && !m.from_me))
            })
            .collect()
    }

    /// Unread messages in the conversations the pill shows.
    pub fn visible_unread(&self, filters: &Filters) -> u32 {
        self.visible(filters).iter().map(|c| c.unread).sum()
    }

    /// A chat dropped from the inbox (the user ignores it).
    pub fn remove(&mut self, chat: &str) {
        self.conversations.remove(&normalized(chat));
    }

    pub fn mark_read(&mut self, chat: &str) {
        if let Some(c) = self.conversations.get_mut(&normalized(chat)) {
            c.unread = 0;
        }
    }

    pub fn rename(&mut self, chat: &str, name: &str) {
        if name.is_empty() {
            return;
        }
        if let Some(c) = self.conversations.get_mut(&normalized(chat)) {
            c.name = name.to_string();
        }
    }

    /// Everything goes (Coucou quits, WhatsApp turned off).
    pub fn clear(&mut self) {
        *self = Inbox::default();
    }
}

// ── What may be sent ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Empty,
    TooLong,
    NotAConversation,
}

/// The gateway's own cap for one text.
pub const MAX_REPLY: usize = 4096;

/// What may be sent from the island: a reply, in a conversation already there, one at a time.
pub fn check_reply(text: &str, chat: &str, inbox: &Inbox) -> Option<Refusal> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Some(Refusal::Empty);
    }
    if trimmed.chars().count() > MAX_REPLY {
        return Some(Refusal::TooLong);
    }
    // Never a new chat, never a list of recipients: only a conversation that wrote to you.
    if inbox.conversation(chat).is_none() {
        return Some(Refusal::NotAConversation);
    }
    None
}

/// A few sends a minute at most, whatever is clicked: Coucou is no bulk sender.
#[derive(Clone, Debug)]
pub struct RateLimit {
    limit: usize,
    window: Duration,
    sent: Vec<Instant>,
}

impl RateLimit {
    pub fn new(limit: usize, window: Duration) -> Self {
        RateLimit { limit, window, sent: Vec::new() }
    }

    pub fn allow(&mut self, now: Instant) -> bool {
        self.sent.retain(|t| now.saturating_duration_since(*t) <= self.window);
        if self.sent.len() >= self.limit {
            return false;
        }
        self.sent.push(now);
        true
    }
}

// ── Hex and base64 ───────────────────────────────────────────────────────────

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Standard base64, padded: the QR's PNG goes to the settings window as a data: URL.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, chat: &str, name: &str, text: &str, date: i64) -> Message {
        Message::new(id, chat, chat, name, text, date)
    }

    #[test]
    fn dates_round_trip_and_read_offsets() {
        // GOWA's docs: 2023-10-15T10:30:00Z.
        assert_eq!(parse_rfc3339("2023-10-15T10:30:00Z"), Some(1_697_365_800));
        assert_eq!(parse_rfc3339("2025-07-13T11:05:51Z"), Some(1_752_404_751));
        assert_eq!(parse_rfc3339("2025-07-13T18:05:51.123+07:00"), Some(1_752_404_751));
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("nope"), None);
        assert_eq!(parse_rfc3339("2025-13-13T11:05:51Z"), None);
        assert_eq!(parse_rfc3339("2025-07-13T11:05:51"), None);
        assert_eq!(iso(1_697_365_800), "2023-10-15T10:30:00Z");
        assert_eq!(parse_rfc3339(&iso(1_752_404_751)), Some(1_752_404_751));
    }

    #[test]
    fn ids_fold_into_one_chat_and_mentions_need_the_whole_number() {
        assert_eq!(normalized("33612345678:12@s.whatsapp.net"), "33612345678@s.whatsapp.net");
        assert_eq!(normalized("33612345678@c.us"), "33612345678@s.whatsapp.net");
        assert_eq!(normalized("120363@g.us"), "120363@g.us");
        assert_eq!(normalized("nope"), "nope");
        assert_eq!(display_number("33612345678@s.whatsapp.net"), "+33612345678");
        assert_eq!(display_number("abc@lid"), "abc");
        assert!(text_mentions("hey @33612345678!", Some("33612345678@s.whatsapp.net")));
        assert!(!text_mentions("hey @336", Some("33612345678@s.whatsapp.net")));
        assert!(!text_mentions("hey @33612345678", None));
    }

    #[test]
    fn one_entry_per_conversation_and_duplicates_are_dropped() {
        let mut inbox = Inbox::default();
        let f = Filters::default();
        assert_eq!(inbox.add(msg("a", "33600000001@s.whatsapp.net", "Marie", "hi", 10), &f, None, false), Outcome::Notify);
        assert_eq!(inbox.add(msg("b", "33600000001@c.us", "Marie", "again", 20), &f, None, false), Outcome::Notify);
        assert_eq!(inbox.add(msg("a", "33600000001@s.whatsapp.net", "Marie", "hi", 10), &f, None, false), Outcome::Duplicate);
        assert_eq!(inbox.sorted().len(), 1);
        let c = inbox.conversation("33600000001@s.whatsapp.net").unwrap();
        assert_eq!((c.unread, c.messages.len(), c.name.as_str()), (2, 2, "Marie"));
        inbox.mark_read("33600000001@c.us");
        assert_eq!(inbox.conversation("33600000001@s.whatsapp.net").unwrap().unread, 0);
    }

    #[test]
    fn only_the_last_twenty_messages_are_kept() {
        let mut inbox = Inbox::default();
        let f = Filters::default();
        for i in 0..30 {
            inbox.add(msg(&format!("m{i}"), "33600000002@s.whatsapp.net", "Léa", "x", 100 + i), &f, None, false);
        }
        let c = inbox.conversation("33600000002@s.whatsapp.net").unwrap();
        assert_eq!(c.messages.len(), Inbox::KEEP);
        assert_eq!(c.messages.first().unwrap().id, "m10");
        assert_eq!(c.unread, 30);
    }

    #[test]
    fn groups_stay_quiet_unless_you_are_mentioned() {
        let mut inbox = Inbox::default();
        let f = Filters::default();
        let group = "120363402106000001@g.us";
        assert_eq!(inbox.add(msg("g1", group, "John", "blah", 1), &f, None, false), Outcome::Quiet);
        assert!(inbox.visible(&f).is_empty());
        let mut mention = msg("g2", group, "John", "@628987654321 tu viens ?", 2);
        mention.mentions_me = true;
        assert_eq!(inbox.add(mention, &f, None, false), Outcome::Notify);
        assert_eq!(inbox.visible(&f).len(), 1);
        // Unmuted: every group message reaches the pill.
        let open = Filters { mute_groups: false, ..Filters::default() };
        assert_eq!(inbox.add(msg("g3", group, "John", "again", 3), &open, None, false), Outcome::Notify);
    }

    #[test]
    fn an_ignored_chat_never_shows() {
        let mut inbox = Inbox::default();
        let mut f = Filters::default();
        f.ignored.insert("33600000004@s.whatsapp.net".into());
        assert_eq!(inbox.add(msg("i1", "33600000004@c.us", "Spam", "buy", 1), &f, None, false), Outcome::Ignored);
        assert!(inbox.sorted().is_empty());
    }

    #[test]
    fn my_own_message_does_not_name_the_chat_after_me() {
        let mut inbox = Inbox::default();
        let f = Filters::default();
        let chat = "33600000003@s.whatsapp.net";
        let mut mine = msg("o1", chat, "Louis", "yo", 5);
        mine.from_me = true;
        assert_eq!(inbox.add(mine, &f, None, false), Outcome::Quiet);
        assert_eq!(inbox.conversation(chat).unwrap().name, "+33600000003");
        inbox.add(msg("o2", chat, "Marie", "yo", 6), &f, None, false);
        assert_eq!(inbox.conversation(chat).unwrap().name, "Marie");
    }

    #[test]
    fn history_is_quiet_and_leaves_unread_alone() {
        let mut inbox = Inbox::default();
        let f = Filters::default();
        let chat = "33600000003@s.whatsapp.net";
        inbox.add(msg("o2", chat, "Marie", "yo", 100), &f, None, false);
        let before = inbox.conversation(chat).unwrap().unread;
        assert_eq!(inbox.add(msg("h1", chat, "Marie", "old", 50), &f, None, true), Outcome::Quiet);
        let c = inbox.conversation(chat).unwrap();
        assert_eq!(c.unread, before);
        assert_eq!(c.messages.first().unwrap().id, "h1");
    }

    #[test]
    fn a_reply_goes_only_to_a_conversation_that_wrote_to_you() {
        let mut inbox = Inbox::default();
        let f = Filters::default();
        let chat = "33600000005@s.whatsapp.net";
        inbox.add(msg("r1", chat, "Marie", "hi", 1), &f, None, false);
        assert_eq!(check_reply("ok", chat, &inbox), None);
        assert_eq!(check_reply("ok", "33600000005@c.us", &inbox), None);
        assert_eq!(check_reply("  \n ", chat, &inbox), Some(Refusal::Empty));
        assert_eq!(check_reply(&"x".repeat(MAX_REPLY + 1), chat, &inbox), Some(Refusal::TooLong));
        assert_eq!(check_reply(&"x".repeat(MAX_REPLY), chat, &inbox), None);
        // Never a new chat: not a stranger, not a group the inbox has not seen.
        assert_eq!(check_reply("ok", "33699999999@s.whatsapp.net", &inbox), Some(Refusal::NotAConversation));
        assert_eq!(check_reply("ok", "120363@g.us", &inbox), Some(Refusal::NotAConversation));
    }

    #[test]
    fn at_most_six_sends_a_minute() {
        let mut limit = RateLimit::new(6, Duration::from_secs(60));
        let t0 = Instant::now();
        for i in 0..6 {
            assert!(limit.allow(t0 + Duration::from_secs(i)), "{i}");
        }
        assert!(!limit.allow(t0 + Duration::from_secs(10)));
        // The window slides: the first send is out of it a minute later.
        assert!(limit.allow(t0 + Duration::from_secs(61)));
    }

    #[test]
    fn base64_is_the_standard_one() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(&[0x89, 0x50, 0x4E, 0x47]), "iVBORw==");
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
        assert_eq!(unhex("000fff"), Some(vec![0, 15, 255]));
        assert_eq!(unhex("0f0"), None);
        assert_eq!(unhex("zz"), None);
    }
}
