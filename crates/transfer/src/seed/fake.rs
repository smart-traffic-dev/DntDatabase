//! Fake English data (spec §4.1 `Fake`), patterns and the unique suffix.

use super::generator::FakeKind;
use super::rng::SplitMix64;
use super::words::*;

fn pick<'a>(list: &[&'a str], rng: &mut SplitMix64) -> &'a str {
    list[rng.below(list.len() as u64) as usize]
}

fn digits(rng: &mut SplitMix64, lo: u64, hi: u64) -> u64 {
    lo + rng.below(hi - lo + 1)
}

fn sentence(rng: &mut SplitMix64) -> String {
    let n = digits(rng, 4, 12);
    let mut out = String::new();
    for i in 0..n {
        let w = pick(WORDS, rng);
        if i == 0 {
            let mut c = w.chars();
            out.extend(c.next().map(|f| f.to_ascii_uppercase()));
            out.push_str(c.as_str());
        } else {
            out.push(' ');
            out.push_str(w);
        }
    }
    out.push('.');
    out
}

/// One fake value of `kind`.
pub fn fake_text(kind: FakeKind, rng: &mut SplitMix64) -> String {
    match kind {
        FakeKind::FirstName => pick(FIRST_NAMES, rng).to_string(),
        FakeKind::LastName => pick(LAST_NAMES, rng).to_string(),
        FakeKind::FullName => format!("{} {}", pick(FIRST_NAMES, rng), pick(LAST_NAMES, rng)),
        FakeKind::Email => format!(
            "{}.{}@{}.{}",
            pick(FIRST_NAMES, rng).to_ascii_lowercase(),
            pick(LAST_NAMES, rng).to_ascii_lowercase(),
            pick(DOMAINS, rng),
            pick(TLDS, rng)
        ),
        FakeKind::Username => {
            let first = pick(FIRST_NAMES, rng).to_ascii_lowercase();
            format!(
                "{}{}",
                &first[..1],
                pick(LAST_NAMES, rng).to_ascii_lowercase()
            )
        }
        FakeKind::Phone => format!(
            "({}) {}-{:04}",
            digits(rng, 200, 999),
            digits(rng, 200, 999),
            digits(rng, 0, 9999)
        ),
        FakeKind::StreetAddress => format!(
            "{} {} {}",
            digits(rng, 1, 9999),
            pick(STREET_NAMES, rng),
            pick(STREET_SUFFIXES, rng)
        ),
        FakeKind::City => CITIES[rng.below(CITIES.len() as u64) as usize]
            .0
            .to_string(),
        FakeKind::State => pick(STATES, rng).to_string(),
        FakeKind::ZipCode => format!("{:05}", digits(rng, 1001, 99_950)),
        FakeKind::Country => pick(COUNTRIES, rng).to_string(),
        FakeKind::Company => format!(
            "{} {}",
            pick(COMPANY_WORDS, rng),
            pick(COMPANY_SUFFIXES, rng)
        ),
        FakeKind::JobTitle => pick(JOB_TITLES, rng).to_string(),
        FakeKind::Url => format!(
            "https://www.{}.{}/{}",
            pick(COMPANY_WORDS, rng).to_ascii_lowercase(),
            pick(TLDS, rng),
            pick(WORDS, rng)
        ),
        FakeKind::Domain => format!(
            "{}.{}",
            pick(COMPANY_WORDS, rng).to_ascii_lowercase(),
            pick(TLDS, rng)
        ),
        FakeKind::Ipv4 => (0..4)
            .map(|_| digits(rng, 1, 254).to_string())
            .collect::<Vec<_>>()
            .join("."),
        FakeKind::Slug => {
            let n = digits(rng, 2, 4);
            (0..n)
                .map(|_| pick(WORDS, rng))
                .collect::<Vec<_>>()
                .join("-")
        }
        FakeKind::Word => pick(WORDS, rng).to_string(),
        FakeKind::Sentence => sentence(rng),
        FakeKind::Paragraph => {
            let n = digits(rng, 2, 5);
            (0..n).map(|_| sentence(rng)).collect::<Vec<_>>().join(" ")
        }
        FakeKind::HexColor => format!("#{:06x}", rng.below(0x100_0000)),
        FakeKind::PasswordHash => {
            const ALPHABET: &[u8] =
                b"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
            let body: String = (0..53)
                .map(|_| ALPHABET[rng.below(ALPHABET.len() as u64) as usize] as char)
                .collect();
            format!("$2b$10${body}")
        }
    }
}

/// `p` with `#` a digit, `?` a letter A–Z, `*` a letter or digit, `\x` a literal `x`.
pub fn pattern(p: &str, rng: &mut SplitMix64) -> String {
    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut out = String::with_capacity(p.len());
    let mut chars = p.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next().unwrap_or('\\')),
            '#' => out.push((b'0' + rng.below(10) as u8) as char),
            '?' => out.push((b'A' + rng.below(26) as u8) as char),
            '*' => out.push(ALNUM[rng.below(ALNUM.len() as u64) as usize] as char),
            c => out.push(c),
        }
    }
    out
}

/// `value` made unique with row number `n`: e-mails before the `@`, usernames appended,
/// anything else after a `-`.
pub fn with_suffix(kind: Option<FakeKind>, value: &str, n: u64) -> String {
    match (kind, value.split_once('@')) {
        (Some(FakeKind::Email), Some((local, domain))) => format!("{local}.{n}@{domain}"),
        (Some(FakeKind::Username), _) => format!("{value}{n}"),
        _ => format!("{value}-{n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seed::rng::SplitMix64;
    use crate::seed::words;

    fn all_lower_alpha(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase())
    }

    #[test]
    fn emails_look_real() {
        let mut rng = SplitMix64::new(1);
        for _ in 0..1000 {
            let e = fake_text(FakeKind::Email, &mut rng);
            let (local, domain) = e.split_once('@').expect("an @");
            let (first, last) = local.split_once('.').expect("first.last");
            assert!(all_lower_alpha(first) && all_lower_alpha(last), "{e}");
            let (host, tld) = domain.split_once('.').expect("a TLD");
            assert!(all_lower_alpha(host), "{e}");
            assert!(["com", "net", "org", "io"].contains(&tld), "{e}");
        }
    }

    #[test]
    fn phones_ipv4_colors_slugs_zips() {
        let mut rng = SplitMix64::new(2);
        for _ in 0..500 {
            let p = fake_text(FakeKind::Phone, &mut rng);
            let b = p.as_bytes();
            assert_eq!(p.len(), 14, "{p}");
            assert_eq!((b[0], b[4], b[5], b[9]), (b'(', b')', b' ', b'-'), "{p}");
            let ip = fake_text(FakeKind::Ipv4, &mut rng);
            let octets: Vec<u32> = ip.split('.').map(|o| o.parse().unwrap()).collect();
            assert_eq!(octets.len(), 4);
            assert!(octets.iter().all(|o| (1..=254).contains(o)), "{ip}");
            let c = fake_text(FakeKind::HexColor, &mut rng);
            assert!(c.len() == 7 && c.starts_with('#'), "{c}");
            assert!(c[1..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
            let s = fake_text(FakeKind::Slug, &mut rng);
            let parts: Vec<&str> = s.split('-').collect();
            assert!((2..=4).contains(&parts.len()), "{s}");
            assert!(parts.iter().all(|p| all_lower_alpha(p)), "{s}");
            let z = fake_text(FakeKind::ZipCode, &mut rng);
            assert!(z.len() == 5 && z.bytes().all(|b| b.is_ascii_digit()), "{z}");
        }
    }

    #[test]
    fn names_from_lists() {
        let mut rng = SplitMix64::new(3);
        for _ in 0..200 {
            let f = fake_text(FakeKind::FirstName, &mut rng);
            assert!(words::FIRST_NAMES.contains(&f.as_str()));
            let full = fake_text(FakeKind::FullName, &mut rng);
            let (a, b) = full.split_once(' ').unwrap();
            assert!(words::FIRST_NAMES.contains(&a) && words::LAST_NAMES.contains(&b));
            let city = fake_text(FakeKind::City, &mut rng);
            assert!(words::CITIES.iter().any(|(c, _)| *c == city));
        }
    }

    #[test]
    fn sentences_and_paragraphs() {
        let mut rng = SplitMix64::new(4);
        for _ in 0..200 {
            let s = fake_text(FakeKind::Sentence, &mut rng);
            assert!(s.chars().next().unwrap().is_ascii_uppercase(), "{s}");
            assert!(s.ends_with('.'));
            let n = s.split(' ').count();
            assert!((4..=12).contains(&n), "{s}");
            let p = fake_text(FakeKind::Paragraph, &mut rng);
            let sentences = p.matches(". ").count() + 1;
            assert!((2..=5).contains(&sentences), "{p}");
        }
    }

    #[test]
    fn password_hash_shape() {
        let mut rng = SplitMix64::new(5);
        let h = fake_text(FakeKind::PasswordHash, &mut rng);
        assert!(h.starts_with("$2b$10$"), "{h}");
        assert_eq!(h.len(), 60);
        assert!(h[7..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'/'));
    }

    #[test]
    fn other_kinds_are_not_empty() {
        let mut rng = SplitMix64::new(6);
        for kind in [
            FakeKind::LastName,
            FakeKind::Username,
            FakeKind::StreetAddress,
            FakeKind::State,
            FakeKind::Country,
            FakeKind::Company,
            FakeKind::JobTitle,
            FakeKind::Url,
            FakeKind::Domain,
            FakeKind::Word,
        ] {
            assert!(!fake_text(kind, &mut rng).is_empty(), "{kind:?}");
        }
        assert!(fake_text(FakeKind::Url, &mut rng).starts_with("https://"));
    }

    #[test]
    fn pattern_tokens() {
        let mut rng = SplitMix64::new(7);
        for _ in 0..200 {
            let s = pattern("SKU-???-#####", &mut rng);
            assert_eq!(s.len(), 13, "{s}");
            assert!(s.starts_with("SKU-") && &s[7..8] == "-");
            assert!(s[4..7].bytes().all(|b| b.is_ascii_uppercase()), "{s}");
            assert!(s[8..].bytes().all(|b| b.is_ascii_digit()), "{s}");
            let star = pattern("*", &mut rng);
            assert!(star
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()));
        }
        assert_eq!(pattern("\\#x", &mut rng), "#x");
        assert_eq!(pattern("a\\", &mut rng), "a\\");
    }

    #[test]
    fn with_suffix_cases() {
        assert_eq!(
            with_suffix(Some(FakeKind::Email), "john.smith@example.com", 7),
            "john.smith.7@example.com"
        );
        assert_eq!(
            with_suffix(Some(FakeKind::Username), "jsmith", 7),
            "jsmith7"
        );
        assert_eq!(with_suffix(Some(FakeKind::City), "Austin", 7), "Austin-7");
        assert_eq!(with_suffix(None, "ABC-123", 12), "ABC-123-12");
    }
}
