#![cfg(target_os = "linux")]

use glib::variant::ToVariant;

#[test]
fn variant_string_iterators_preserve_values_in_both_directions() {
    // RUSTSEC-2024-0429: an immutable FFI out-pointer becomes NULL under optimization.
    let values = ["first", "Grüße", "last"];
    let variant = values.as_slice().to_variant();
    let mut iter = variant.array_iter_str().expect("string array iterator");
    assert_eq!(iter.next(), Some("first"));
    assert_eq!(iter.next_back(), Some("last"));
    assert_eq!(iter.next(), Some("Grüße"));
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
    assert_eq!(variant.array_iter_str().unwrap().nth(1), Some("Grüße"));
    assert_eq!(variant.array_iter_str().unwrap().nth_back(1), Some("Grüße"));
    assert_eq!(variant.array_iter_str().unwrap().last(), Some("last"));
    assert_eq!(
        variant.array_iter_str().unwrap().collect::<Vec<_>>(),
        values
    );
}
