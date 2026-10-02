//! Natural, case-insensitive ordering: `file2` before `file10`.

use std::cmp::Ordering;
use std::iter::Peekable;
use std::str::Chars;

pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut a = a.chars().peekable();
    let mut b = b.chars().peekable();
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                let ordering = cmp_numbers(&mut a, &mut b);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            (Some(ca), Some(cb)) => {
                a.next();
                b.next();
                let ordering = cmp_letters(ca, cb);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}

/// Digit runs compare by numeric value, so `2` < `10`. The same value with
/// more leading zeros sorts later (`2` then `02`), which keeps the order total.
fn cmp_numbers(a: &mut Peekable<Chars<'_>>, b: &mut Peekable<Chars<'_>>) -> Ordering {
    let mut a_zeros = 0;
    let mut b_zeros = 0;
    while a.peek() == Some(&'0') {
        a.next();
        a_zeros += 1;
    }
    while b.peek() == Some(&'0') {
        b.next();
        b_zeros += 1;
    }
    let mut a_digits = String::new();
    let mut b_digits = String::new();
    while a.peek().is_some_and(|c| c.is_ascii_digit()) {
        a_digits.push(a.next().unwrap());
    }
    while b.peek().is_some_and(|c| c.is_ascii_digit()) {
        b_digits.push(b.next().unwrap());
    }
    let by_value = a_digits
        .len()
        .cmp(&b_digits.len())
        .then_with(|| a_digits.cmp(&b_digits));
    if by_value != Ordering::Equal {
        return by_value;
    }
    a_zeros.cmp(&b_zeros)
}

fn cmp_letters(a: char, b: char) -> Ordering {
    let mut a = a.to_lowercase();
    let mut b = b.to_lowercase();
    loop {
        match (a.next(), b.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let ordering = x.cmp(&y);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_compare_numerically_and_letters_ignore_case() {
        assert_eq!(natural_cmp("file2", "file10"), Ordering::Less);
        assert_eq!(natural_cmp("file2.md", "file10.md"), Ordering::Less);
        assert_eq!(natural_cmp("2.md", "10.md"), Ordering::Less);
        assert_eq!(natural_cmp("File2", "file10"), Ordering::Less);
        assert_eq!(natural_cmp("fileA", "FileB"), Ordering::Less);
        assert_eq!(natural_cmp("file2", "file02"), Ordering::Less);
        assert_eq!(natural_cmp("readme", "Readme"), Ordering::Equal);
    }
}
