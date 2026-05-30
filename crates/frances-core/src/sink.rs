//! Lightweight [`Extend`] sinks.

/// An [`Extend`] sink that counts items instead of storing them.
///
/// Lets a routine that normally fills a `Vec` run allocation-free when the
/// caller only needs the count — e.g. measuring wrapped-line height without
/// materialising the lines.
pub struct CountingSink(pub usize);

impl<T> Extend<T> for CountingSink {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        self.0 += iter.into_iter().count();
    }
}

#[cfg(test)]
mod tests {
    use super::CountingSink;

    #[test]
    fn counts_extended_items() {
        let mut sink = CountingSink(0);
        sink.extend([1, 2, 3]);
        sink.extend(std::iter::once(4));
        assert_eq!(sink.0, 4);
    }
}
