#!/usr/bin/env bash
# Regenerates words.txt: capitalised English words for the anchor dictionary.
# DICT_SIZE entries total: N_PADDING_WORDS reserved positions at the start
# (their text doesn't matter — they're matched by index) plus N_DATA_WORDS
# usable data words, sized to fit BITS_PER_WORD bits per word. Keep these
# constants in sync with crates/frances-anchors/src/words.rs.
#
# Words are filtered to dodge the worst classes of model-induced anchor
# errors and to minimise prompt token cost:
#   * 1 token only — keeps anchors as cheap as possible and biases towards
#     words common enough that the model types them accurately.
#   * length >= 4 — short words are autoregressive truncation targets ("He"
#     emitted instead of "His") and confuse easily with prose tokens.
#   * prefix-collision — if both "Wil" and "Will" were kept, the model could
#     truncate "Will" to "Wil" and we'd silently look up the wrong line. The
#     filter ensures no kept word is a prefix of any other kept word, so any
#     truncation lands outside the dictionary and gets caught loudly.
#
# Writes to words-anchors.txt in the current directory; rename to words.txt
# to adopt. Each run also appends a dated entry to token-stats.txt — this is
# the metric that really matters, since cheaper tokens per anchor word means
# cheaper session prompts.
set -euo pipefail

nix run nixpkgs#uv -- run --with tiktoken --with wordfreq --python 3.12 - <<'EOF'
import bisect
import datetime
import tiktoken
from wordfreq import top_n_list
from collections import Counter

BITS_PER_WORD = 11
N_PADDING_WORDS = BITS_PER_WORD - 1  # one per possible residual bit count
N_DATA_WORDS = 1 << BITS_PER_WORD
TARGET = N_PADDING_WORDS + N_DATA_WORDS  # 2058

MIN_LEN = 4
MAX_TOKENS = 1
SOURCE_POOL = 100000  # plenty of headroom after filters

encs = [tiktoken.get_encoding(n) for n in ("cl100k_base", "o200k_base")]

raw = top_n_list("en", SOURCE_POOL)
words = [w for w in raw if w.isalpha() and len(w) >= MIN_LEN]

def cost(w):
    return max(len(e.encode(w)) for e in encs)

# Single-token capitalised forms only, in source-frequency order so the
# resulting list is also frequency-ranked (helpful for humans skimming it).
candidates = []
for i, w in enumerate(words):
    cap = w.capitalize()
    c = cost(cap)
    if c <= MAX_TOKENS:
        candidates.append((c, i, cap))

# Greedy prefix-free selection in candidate order. `kept_set` answers "does
# some kept word equal a prefix of `w`?" in O(len(w)). `kept_sorted` answers
# "is `w` a prefix of some kept word?" via a bisect — the smallest kept word
# that's >= w lexically is either w itself or a word that starts with w.
kept_set = set()
kept_sorted = []
prefix_collisions = 0
for _, _, w in candidates:
    if any(w[:i] in kept_set for i in range(MIN_LEN, len(w))):
        prefix_collisions += 1
        continue
    idx = bisect.bisect_left(kept_sorted, w)
    if idx < len(kept_sorted) and kept_sorted[idx].startswith(w):
        prefix_collisions += 1
        continue
    kept_set.add(w)
    bisect.insort(kept_sorted, w)
    if len(kept_set) == TARGET:
        break

if len(kept_set) < TARGET:
    raise SystemExit(
        f"only found {len(kept_set)} {MAX_TOKENS}-token prefix-free words; "
        f"need {TARGET}. Raise SOURCE_POOL or relax MAX_TOKENS."
    )

keep = [(c, i, w) for c, i, w in candidates if w in kept_set]
dist = dict(sorted(Counter(t[0] for t in keep).items()))

stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
lines = [
    f"# {stamp}",
    f"input: {len(words)} words (from top {SOURCE_POOL}, len>={MIN_LEN}, alpha)",
    f"target dict size: {TARGET} ({N_PADDING_WORDS} padding + {N_DATA_WORDS} data, {BITS_PER_WORD} bits/word)",
    f"prefix-collision drops: {prefix_collisions}",
    f"token-count distribution in kept set: {dist}",
    "",
]
report = "\n".join(lines)
print(report, end="")

with open("token-stats.txt", "a") as f:
    f.write(report)

with open("words-anchors.txt", "w") as f:
    f.write("\n".join(t[2] for t in keep) + "\n")
EOF
