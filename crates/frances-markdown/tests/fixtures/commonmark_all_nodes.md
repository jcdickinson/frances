# ATX heading level 1

## ATX heading level 2

### ATX heading level 3

#### ATX heading level 4

##### ATX heading level 5

###### ATX heading level 6

## Closed ATX heading ##

Setext heading, level 1
=======================

Setext heading, level 2
-----------------------

A plain paragraph with a trailing hard break produced by two spaces  
and a second line, plus a hard break via a backslash\
and a third line. It also has *emphasis*, _underscore emphasis_,
**strong**, __underscore strong__, ***both at once***, and an
`inline code span`, plus a ``span with ` backtick`` inside it.

An inline [link](https://example.com "with a title"), a bare autolink
<https://example.com/auto>, an email autolink <person@example.com>, an
inline image ![alt text](/img.png "image title"), an escaped \*literal
asterisk\*, and an entity reference &amp; plus &#36; numeric one.

A full reference [link][ref-full], a collapsed reference [ref-collapsed][],
a shortcut reference [ref-shortcut], a reference image ![img][ref-image],
and a collapsed reference image ![ref-image][].

***

---

___

> A block quote.
>
> > Nested block quote with **strong** inside.
>
> Back to the outer level, with a lazy
> continuation line.

- Unordered list with a dash
- second item with *emphasis*
  - nested item indented two spaces
- third item

+ Unordered list with a plus marker
+ second plus item

* Unordered list with a star marker
* second star item

1. Ordered list, period delimiter
2. second ordered item
   1. nested ordered item
3. third ordered item

1) Ordered list, paren delimiter
2) second paren item

5. Ordered list starting at five

- A loose list item with its own paragraph.

- Followed by a blank line, so the list is loose.

```rust
fn fenced() {
    println!("backtick fence with an info string");
}
```

~~~
tilde fence with no info string
~~~

    an indented code block
    spanning two lines

A paragraph between two indented blocks so the second one is unambiguous.

    a second indented code block
    with an interior blank line:

    final indented line after the blank

<div class="html-block">
  A raw HTML block.
</div>

A paragraph containing <span class="inline-html">inline raw HTML</span> too.

[ref-full]: https://example.com/full "full reference title"
[ref-collapsed]: https://example.com/collapsed
[ref-shortcut]: https://example.com/shortcut
[ref-image]: /image-ref.png "image reference title"
