# The Bao Spec

**Caution:** Bao is intended to be a cryptographic hash function, but it hasn't
yet been reviewed. The output may change prior to the 1.0 release.

## Contents

* [Tree Structure](#tree-structure)
* [Combined Encoding Format](#combined-encoding-format)
* [Outboard Encoding Format](#outboard-encoding-format)
* [Slice Format](#slice-format)
* [Security](#security)
* [Storage Requirements](#storage-requirements)
* [Performance Notes](#performance-notes)
* [Design Rationales and Open Questions](#design-rationales-and-open-questions)
* [Other Related Work](#other-related-work)


## Tree Structure

Bao divides the input up into 4096-byte chunks. The final chunk may be shorter,
but it's never empty unless the input itself is empty. The chunks are arranged
as the leaves of a binary tree. All parent nodes have exactly two children, and
the content of each parent node is the hash of its left child concatenated with
the hash of its right child. When there's an odd number of nodes in a given
level of the tree, the rightmost node is raised to the level above. Here's what
the tree looks like as it grows from 1 to 4 chunks.

```
                                         <parent>                       <parent>
                                          /    \                      /          \
                <parent>            <parent>  [CHUNK3]         <parent>           <parent>
                 /   \               /   \                     /   \              /   \
[CHUNK1]   [CHUNK1] [CHUNK2]   [CHUNK1] [CHUNK2]         [CHUNK1] [CHUNK2]  [CHUNK3] [CHUNK4]
```

We can also describe the tree recursively:

- If a tree/subtree contains 4096 input bytes or less, the tree/subtree is just
  a chunk.
- Otherwise, the tree/subtree is rooted at a parent node, and its input bytes
  are divided between its left and right child subtrees. The number of input
  bytes on the left is largest power of 2 times 4096 that's strictly less than
  the total. The remainder, always at least 1 byte, goes on the right.

Hashing nodes is done with BLAKE2b, using the following parameters:

- **Hash length** is 32.
- **Fanout** is 2.
- **Max depth** is 64.
- **Max leaf length** is 4096.
- **Node offset** is always 0 (the default).
- **Node depth** is 0 for all chunks and 1 for all parent nodes.
- **Inner hash length** is 32.

In addition, the root node -- whether it's a chunk or a parent -- is hashed
with two extra steps:

- The total input byte length, encoded as an 8-byte little-endian integer, is
  appended to the root node.
- The **last node** BLAKE2 finalization flag is set to true. Note that BLAKE2
  supports setting the last node flag at any time, so hashing the first chunk
  can begin without knowing whether it's the root.

That root node hash is the output of Bao. Here's an example tree, with 8193
bytes of input that are all zero:

```
                        root parent hash=bed2e4...
                        <1926c3...f330e9...>
                                /   \
                               /     \
            parent hash=1926c3...   chunk hash=f330e9...
            <7fbd4a...7fbd4a...>    [\x00]
                    /   \
                   /     \
chunk hash: 7fbd4a...   chunk hash: 7fbd4a...
[\x00 * 4096]           [\x00 * 4096]
```

We can verify those values on the command line using the `b2sum` utility from
[`blake2b_simd`](https://github.com/oconnor663/blake2b_simd), which supports
the necessary flags (the coreutils `b2sum` doesn't expose all the BLAKE2
parameters):

```bash
# Define some aliases for hashing nodes. Note that --length and
# --inner-hash-length are in bits, not bytes, for compatibility with coreutils.
$ alias hash_node='b2sum --length=256 --fanout=2 --max-depth=64 --max-leaf-length=4096 --inner-hash-length=256'
$ alias hash_chunk='hash_node --node-depth=0'
$ alias hash_parent='hash_node --node-depth=1'

# Compute the hash of the first and second chunks, which are the same.
$ head -c 4096 /dev/zero | hash_chunk
7fbd4a4dce97d0ed509a76448227aac527cb31e20d03096ea360f974b53d8808  -
$ big_chunk_hash=7fbd4a4dce97d0ed509a76448227aac527cb31e20d03096ea360f974b53d8808

# Compute the hash of the third chunk, which is different.
$ head -c 1 /dev/zero | hash_chunk
f330e9ad408a5f3ff2842b45948730c91a3f4d81f98526400ea7e9ba877dcdb3  -
$ small_chunk_hash=f330e9ad408a5f3ff2842b45948730c91a3f4d81f98526400ea7e9ba877dcdb3

# Define an alias for parsing hex.
$ alias unhex='python3 -c "import sys, binascii; sys.stdout.buffer.write(binascii.unhexlify(sys.argv[1]))"'

# Compute the hash of the first two chunks' parent node.
$ unhex $big_chunk_hash$big_chunk_hash | hash_parent
1926c3048e0391cdac5a0b116bd63e03a307e2c10d745b25d24c558e8be2bec9  -
$ left_parent_hash=1926c3048e0391cdac5a0b116bd63e03a307e2c10d745b25d24c558e8be2bec9

# Define another alias converting the input length to 8-byte little-endian hex.
$ alias hexint='python3 -c "import sys; print(int(sys.argv[1]).to_bytes(8, \"little\").hex())"'

# Compute the hash of the root node, with the length suffix and last node flag.
$ unhex $left_parent_hash$small_chunk_hash$(hexint 8193) | hash_parent --last-node
bed2e488d2644ce514036824dd5486c0ad16bd1d4b9ee8e9940f810d8c40284e  -

# Verify that this matches the Bao hash of the same input.
$ head -c 8193 /dev/zero | bao hash
bed2e488d2644ce514036824dd5486c0ad16bd1d4b9ee8e9940f810d8c40284e
```

## Combined Encoding Format

The combined encoding file format is the contents of the chunks and parent
nodes of the tree concatenated together in pre-order (that is a parent,
followed by its left subtree, followed by its right subtree), with the 8-byte
little-endian unsigned input length prepended to the very front. This makes the
order of nodes on disk the same as the order in which a depth-first traversal
would encounter them, so a reader decoding the tree from beginning to end
doesn't need to do any seeking. Here's the same example tree above, formatted
as an encoded file:

```
input length    |root parent node  |left parent node  |first chunk|second chunk|last chunk
0120000000000000|1926c3...f330e9...|7fbd4a...7fbd4a...|000000...  |000000...   |00
```

Note carefully that the first two sections are the reverse of how the root node
is hashed. Hashing the root node *appends* the length as associated data, which
makes it possible to start hashing the first chunk before knowing whether its
the root. Encoding *prepends* the length, because it's the first thing that the
decoder needs to know.

The decoder first reads the 8-byte length from the front. The length indicates
whether the first node is a chunk (<=4096) or a parent (>4096), and it verifies
the hash of root node with the length as associated data. The rest of the tree
structure is completely determined by the length, and the decoder can stream
the whole tree or seek around as needed by the caller. But note that all
decoding operations *must* verify the root. In particular, if the caller asks
to seek to byte 5000 of a 4096-byte encoding, the decoder *must not* skip
verifying the first (only) chunk, because its the root. This ensures that a
decoder will always return an error when the encoded length doesn't match the
root hash

Because of the prepended length, the encoding format is self-delimiting. Most
decoders won't read an encoded file all the way to EOF, and so it's generally
allowed to append extra garbage bytes to a valid encoded file. Trailing garbage
has no effect on the content, but it's worth clarifying what is and isn't
guaranteed by the encoding format:

- There is only one input (if any) for a given hash. If the Bao hash of a given
  input is used in decoding, it will never successfully decode anything other
  than exactly that input. Corruptions in the encoding might lead to partial
  output followed by an error, but any partial output will always be a prefix
  of the original input.
- There is only one hash for a given input. There is at most one hash that can
  decode any content, even partial content followed by an error, from a given
  encoding. If the decoding is complete, that hash is always the Bao hash of
  the decoded content. If two decoding hashes are different, then any content
  they successfully and completely decode is always different.
- However, multiple "different" encoded files can decode using the same hash,
  if they differ only in their trailing garbage. So while there's a unique hash
  for any given input, there's not a unique valid encoded file, and comparing
  encoded files with each other is probably a mistake.

## Outboard Encoding Format

The outboard encoding format is the same as the combined encoding format,
except that all the chunks are omitted. Whenever the decoder would read a
chunk, it instead reads the corresponding offset from the original input file.
This is intended for situations where the benefit of retaining the unmodified
input file is worth the complexity of reading two separate files to decode.

## Slice Format

The slice format is very similar to the combined encoding format above. The
only difference is that chunks and parent nodes that wouldn't be encountered in
a given traversal are omitted. For example, if we slice the tree above starting
at input byte 4096 (the beginning of the second chunk), and request any count
of bytes less than or equal to 4096 (up to the end of that chunk), the
resulting slice will be this:

```
input length    |root parent node  |left parent node  |second chunk
0120000000000000|1926c3...f330e9...|7fbd4a...7fbd4a...|000000...
```

Although slices can be extracted from either a combined encoding or an outboard
encoding, there is no such thing as an "outboard slice". Slices always include
chunks inline, as the combined encoding does. A slice that includes the entire
input is exactly the same as the combined encoding of that input.

Decoding a slice works just like decoding a combined encoding. The only
difference is that in cases where the decoder would normally seek forward, the
slice decoder continues reading in series, since all the seeking has been taken
care of by the slice extractor.

There are some unspecified edge cases in the slice parameters:

- A starting point past the end of the input.
- A byte count larger than the available input after the starting point.
- A byte count of zero.

A future version of the spec will settle on the behavior in these cases.

## Security

When designing a tree mode, there are pitfalls that can compromise the security
of the underlying hash. For example, if one input produces a tree with bytes X
at the root node, and we choose another input to be those same bytes X, do
those two inputs result in the same root hash? If so, that's a hash collision,
regardless of the security of the underlying hash function. Or if one input
results in a root hash Y, could Y be incorporated as a subtree hash in another
tree without knowing the input that produced it? If so, that's a length
extension, again regardless of the properties of the underlying hash. There are
many possible variants of these problems.

[*Sufficient conditions for sound tree and sequential hashing
modes*](https://eprint.iacr.org/2009/210.pdf) (2009), authored by the
Keccak/SHA-3 team, lays out a minimal set of requirements for a tree mode, to
prevent attacks like the above. This section describes how Bao satisfies those
requirements. They are:

1. **Tree decodability.** The exact definition of this property is fairly
   technical, but the gist of it is that it needs to be impossible to take a
   valid tree, add more child nodes to it somewhere, and wind up with another
   valid tree.
2. **Message completeness.** It needs to be possible to reconstruct the
   original message from the tree.
3. **Final-node separability.** Again the exact definition is fairly technical,
   but the gist is that it needs to be impossible for any root node and any
   non-root node to have the same hash.

We ensure **tree decodability** by domain-separating parent nodes from leaf
nodes (chunks) with the **node depth** parameter. BLAKE2's parameters are
functionally similar to the frame bits used in the paper, in that two inputs
with different parameters always produce a different hash, though the
parameters are implemented as tweaks to the IV rather than by concatenating
them with the input. Because chunks are domain-separated from parent nodes,
adding children to a chunk is always invalid. That, coupled with the fact that
parent nodes are always full and never have room for more children, means that
adding nodes to a valid tree is always invalid.

Note that we could have established tree decodability without relying on domain
separation, by referring to the length counter appended to the root node. Since
the layout of the tree is entirely determined by the input length, adding
children without changing the length can never be valid. However, this approach
raises troubling questions about what would happen if the length counter
overflowed. It's easy to say in theory that such trees would be invalid, but
implementations in the real world might tend to produce them rather than
aborting, and that could lead to collisions in practice. Although the 8-byte
counter is large enough that overflowing it is unrealistic for most
implementations, it's within reach of powerful distributed systems like the one
Google Research used to break SHA-1. A clever sparse file application could
also exploit symmetry in the interior of the tree to hash an astronomically
large file of mostly zeros (more discussion of sparse files in the Design
Rationales below). Domain separation avoids all of these problems, and in
BLAKE2 it has no performance cost.

**Message completeness** is of course a basic design requirement of the
encoding format, and all the bits of the format are included in the tree. (The
format swaps the position of the length counter to the front, which makes it
possible to reconstruct the tree from a flat file, but it doesn't include any
extra bits.) Message completeness would hold even without appending the input
length to the root node, because the input would be the concatenation of all
the leaves.

We ensure **final-node separability** by domain-separating the root node from
the rest of the tree with the **final node flag**. BLAKE2's final node flag is
similar to its other parameters, except that it's an input to the last call to
the compression function rather than a tweak to the IVs. In practice, that
allows an implementation to start hashing the first chunk immediately rather
than buffering it, and to set the final node flag at the end if the first chunk
turns out to be the only chunk and therefore the root. This is also why hashing
appends the length counter to the root rather than prepending it.

## Storage Requirements

A Bao implementation needs to store one hash (32 bytes) for every level of the
tree. The largest supported input is 2<sup>64</sup> - 1 bytes. Given the
4096-byte chunk size (2<sup>12</sup>), that's 2<sup>52</sup> leaf nodes, or a
maximum tree height of 52. Storing 52 hashes, 32 bytes each, requires 1664
bytes, in addition to the [336 bytes](https://blake2.net/blake2.pdf) required
by BLAKE2b. For comparison, the TLS record buffer is 16384 bytes.

Extremely space-constrained implementations that want to use Bao have to define
a more aggressive limit for their maximum input size. In some cases, such a
limit is already provided by the protocol they're implementing. For example,
the largest possible IPv6 "jumbogram" is 4 GiB, and limited to that maximum
input size Bao's storage overhead would be 20 hashes or 640 bytes.

## Performance Notes

To get the highest possible throughput, the Bao implementation uses both
threads and SIMD. Threads let the computation take advantage of multiple CPU
cores, and SIMD gives each thread a higher overall throughput by hashing
multiple chunks at once.

Multithreading in the current implementation is done with the
[`join`](https://docs.rs/rayon/latest/rayon/fn.join.html) function from the
[Rayon](https://github.com/rayon-rs/rayon) library. It splits up its input
recursively -- an excellent fit for traversing a tree -- and allows worker
threads to "steal" the right half of the split, if they happen to be free. Once
the global thread pool is initialized, this approach doesn't require any heap
allocations.

There are two different approaches to using SIMD to speed up BLAKE2b. The more
common way is to optimize a single instance, and that's the approach that eeks
past SHA-1 in the [BLAKE2b benchmarks](https://blake2.net/). But the more
efficient way, when you have multiple inputs, is to run multiple instances in
parallel on a single thread. Samuel Neves discussed the second approach in [a
2012 paper](https://eprint.iacr.org/2012/275.pdf) and implemented it in the
[reference AVX2 implementation of
BLAKE2bp](https://github.com/sneves/blake2-avx2/blob/master/blake2bp.c). The
overall throughput is about double that of a single instance. The
[`blake2b_simd`](https://github.com/oconnor663/blake2b_simd) implementation
includes an
[`update4`](https://docs.rs/blake2b_simd/latest/blake2b_simd/fn.update4.html)
interface, which provides the same speedup for multiple instances of BLAKE2b,
and Bao uses that interface to make each worker thread hash four chunks in
parallel. Note that the main downside of BLAKE2bp is that it hardcodes 4-way
parallelism, such that moving to a higher degree of parallelism would change
the output. But multi-way-BLAKE2b doesn't have that limitation, and when 8-way
SIMD becomes more widespread, Bao can swap out `update4` for `update8`.

## Design Rationales and Open Questions

### Can we expose the BLAKE2 general parameters through the Bao API?

**Yes, though there are some design choices we need to make.** The general
parameters are the variable output length, secret key, salt, and
personalization string. A future version of this spec will almost certainly
settle on a way to expose them. The salt and personalization will probably be
simple; just apply them to all nodes in the tree.

The right approach for the secret key is less clear. The BLAKE2 spec says:
"Note that tree hashing may be keyed, in which case leaf instances hash the key
followed by a number of bytes equal to (at most) the maximal leaf length." That
remark actually leaves the trickiest detail unsaid, which is that while only
the leaf nodes hash the key bytes, _all_ nodes include the key length as
associated data. This is behavior is visible in the BLAKE2bp [reference
implementation](https://github.com/BLAKE2/BLAKE2/blob/a90684ab3fe788b2ca45076cf9b38335de289f58/ref/blake2bp-ref.c#L65)
and confirmed by its test vectors. Unfortunately, this behavior is actually
impossible to implement with Python's `hashlib.blake2b` API, which ties the key
length and key bytes together. An approach that applied the key bytes to every
node would fit into Python's API, but it would both depart from the spec
conventions and add extra overhead. Implementing Bao in pure Python isn't
necessarily a requirement, but the majority of BLAKE2 implementations in the
wild have similar limitations.

The variable output length has a similar issue. In BLAKE2bp, the root node's
output length is the hash length parameter, and the leaf nodes' output length
is the inner hash length parameter, with those two parameters set the same way
for all nodes. That's again impossible in the Python API, where the output
length and the hash length parameter are always set together. Bao has the same
problem, because the interior subtree hashes are always 32 bytes. Also, a
64-byte Bao output would have the same effective security as the 32-byte
output, so it might be misleading to even offer the longer version.

### Why not use the full 64 bytes of BLAKE2b output in the interior of the tree?

**Storage overhead.** Note that in the [Storage
Requirements](#storage-requirements), the storage overhead is proportional to
the size of a subtree hash. Storing 64-byte hashes would double the overhead.
This does mean that BLAKE2b's 256 bits of collision resistance is reduced to
128 bits, even if a future extension makes the full 64-byte root output
available.

It's worth noting that BLAKE2b's block size is 128 bytes, so hashing a parent
node takes the same amount of time whether the child hashes are 32 bytes or 64.
However, the 32-byte size does leave room in the block for the root length
suffix.

### Should we stick closer to the BLAKE2 spec when setting node offset and node depth?

**Probaby not.** In the [BLAKE2 spec](https://blake2.net/blake2.pdf), it was
originally intended that each node would use its unique depth/offset pair as
parameters to the hash function. The Security section above made the case that
that approach isn't necessary to prevent collisions, but there could still be
some value in sticking to the letter of the spec. There are a few reasons Bao
doesn't.

One reason is that, by allowing identical subtrees to produce the same hash,
Bao makes it possible to do interesting things with sparse files. For example,
imagine you need to compute the hash of an entire disk, but you know that most
of the disk contains all zeros. You can skip most of the work of hashing it, by
memoizing the hashes of all-zero subtrees. That approach works with any pattern
of bytes that repeats on a subtree boundary. But if we set the node offset
parameter differently in every subtree, memoizing no longer helps.

Also, we're already departing from the BLAKE2 spec in our use of the last node
flag. The spec intended it to be set for all nodes on the right edge of the
tree, but we only set it for the root node. It doesn't seem worth it to make
implementations do more bookkeeping to be slightly-more-but-still-not-entirely
compliant with the spec.

### Could we use a simpler tree mode, like KangarooTwelve does?

**No, the encoding format requires a full tree.**
[KangarooTwelve](https://keccak.team/kangarootwelve.html) is a modern hash
function based on Keccak/SHA3, and it includes a ["leaves stapled to a
pole"](https://www.cryptologie.net/article/393/kangarootwelve) tree internally
to allow for parallelism in the implementation. This is much simpler to
implement than a full binary tree, and it adds less storage overhead.

However, a shallow tree would limit the usefulness of Bao's encoding and
slicing features. The root node becomes linear in the size of the input.
Encoding a gigabyte file, for example, would produce a root node that's several
megabytes. The recipient would need to fetch and buffer the entire root before
verifying any content bytes, and decoding would require heap allocation. The
usefulness of the encoding format would be limited to the space of files big
enough that streaming is valuable, but small enough that the root node is
manageable, and it would preclude most embedded applications. Incremental
update schemes would suffer too, because every update would need to rehash the
large root node.

A two-level tree would also limit parallelism. As noted in the [KangarooTwelve
paper](https://eprint.iacr.org/2016/770.pdf), given enough worker threads
hashing input chunks and adding their hashes to the root, the thread
responsible for hashing the root eventually reaches its own throughput limit.
This happens at a parallelism degree equal to the ratio of the chunk size and
the hash length, 256 in the case of KangarooTwelve. That sounds like an
extraordinary number of threads, but consider that one of Bao's benchmarks is
running on a 96-logical-core AWS machine, and that Bao uses an AVX2
implementation of BLAKE2b that hashes 4 chunks in parallel per thread. That
benchmark is hitting parallelism degree 384 today. Also consider that Intel's
upcoming Cannon Lake generation of processors will probably support the AVX-512
instruction set (8-way SIMD) on 16 logical cores, for a parallelism degree of
128 on a mainstream desktop processor. Now to be fair, this arithmetic is
cheating badly, because logical cores aren't physical cores, and because
hashing 4 inputs with SIMD isn't 4x as fast as hashing 1 input. But the real
throughput of that AWS benchmark is about 90x the max throughput of a single
node, so 256x is only a few hardware generations away.

### Should we fall back to serial hashing for messages above some maximum size?

**No.** Many tree modes, including some described in the [BLAKE2
spec](https://blake2.net/blake2.pdf), fall back to a serial mode after some
threshold. That allows them to specify a small maximum tree height for reduced
memory requirements. However, one of Bao's main benefits is parallelism over
huge files, and falling back to serial hashing would conflict with that. It
would also complicate encoding and decoding.

### What's the best way to choose the chunk size?

**Open question.** I chose 4096 somewhat arbitrarily, because it seems to be a
common page size, and because the performance overhead seems subjectively small
in testing. But there are many efficiency tradeoffs at the margins, and we
might not be able to settle this question without more real world
implementation experience. See [issue #17](https://github.com/oconnor663/bao/issues/17).

While Bao is intended to be a "one size fits all" hash function, the chunk size
is the parameter that different implementations are most likely to need to
tweak. For example, an embedded implementation that implements decoding (will
there ever be such a thing?) needs to allocate buffer space for an entire
chunk. It's possible that a tiny chunk size would be a hard requirement, and
that cutting the overall throughput by a large factor might not matter.

If an implementation needs to customize the chunk size, it will of course break
compatibility with standard Bao. Such an implementation _must_ set the **max
leaf length** parameter accordingly to avoid any chance of causing collisions.
But note that these variants shouldn't change the max depth; that parameter
only represents the size of the input byte count.

### Would it be more efficient to use an arity larger than 2?

**Maybe, but it would add storage overhead.** There's an efficiency argument
for allowing parent nodes to have 4 children. As noted above, the BLAKE2b block
size is 128 bytes. If we're using hashes that are 32 bytes long, hashing a
parent with 2 children takes just as much time as hashing a parent with 4
children, assuming there are no extra suffixes. That would cut the total number
of parent nodes down by a factor of 3 (because 1/4 + 1/16 + ... = 1/3), with
potentially no additional cost per parent.

However, the storage overhead of this design is actually higher than with the
binary tree. While a 4-ary tree is half as tall as a binary tree over the same
number of chunks, its stack needs space for 3 subtree hashes per level, making
the total overhead 3/2 times as large. Also, a 4-ary tree would substantially
complicate the algorithms for merging partial subtrees and computing encoded
offsets. Overall, the cost of hashing parent nodes is already designed to be
small, and shrinking it further isn't worth these tradeoffs.

### Is 64 bits large enough for the length counter?

**Yes.** Every filesystem in use today has a maximum file size of
2<sup>64</sup> bytes or less. It's possible that some trickery with sparse
files (more discussion above) might let you effectively hash something that
large, but at that point there's no practical limit to your input size and no
particular reason to assume that 2<sup>128</sup> or 2<sup>256</sup> bytes would
be enough.

Bao's decoding features are designed to work with the IO interfaces of
mainstream programming languages, particularly around streaming and seeking.
These interfaces are [usually
restricted](https://doc.rust-lang.org/std/io/enum.SeekFrom.html) to 64-bit
sizes and offsets. If Bao supported longer streams in theory, implementations
would need to handle more unrepresentable edge cases. (Though even with a
64-bit counter, the maximum _encoded_ file size can exceed 64 bits, and a
perfect decoder implementation needs to seek twice to reach bytes near the end
of max-size encodings. In practice the decoder returns an overflow error.)

Implementations also need to decide how much storage overhead is reasonable. If
the counter was 128 bits, it would still make almost no sense to allocate space
for a 128-level tree. The recommended default would probably be to assume a
maximum of 52 levels like today, but it would put the burden of choice on each
implementation.

### Could a similar design be based on a different underlying hash function?

**Yes, as long as the underlying hash prevents length extension.** SHA-256 and
SHA-512 aren't suitable, but SHA-512/256 and SHA-3 could be.

Domain separation between the root and non-root nodes, and between chunks and
parent nodes, is a security requirement. For hash functions without associated
data parameters, you can achieve domain separation with a small amount of
overhead by appending some bits to every node. See for example the [Sakura
coding](https://keccak.team/files/Sakura.pdf), also designed by the
Keccak/SHA-3 team.

## Other Related Work

- The [Tree Hash
  Exchange](https://adc.sourceforge.io/draft-jchapweske-thex-02.html) format
  (2003). THEX and Bao have similar tree structures, and both specify a binary
  format for encoding the tree to enable incremental decoding. THEX uses a
  breadth-first rather than depth-first encoding layout, however, which makes
  the decoder's storage requirements much larger. Also, as noted by the
  Keccak/SHA-3 team in [*Sufficient
  conditions*](https://eprint.iacr.org/2009/210.pdf), THEX doesn't
  domain-separate its root node, so it's vulnerable to length extension
  regardless of the security of the underlying hash function.
- [Tree
  Hashing](https://www.cryptolux.org/mediawiki-esc2013/images/c/ca/SL_tree_hashing_esc.pdf)
  (2013), a presentation by Stefan Lucks, discussing the requirements for
  standardizing a tree hashing mode.
- [RFC 6962](https://tools.ietf.org/html/rfc6962) uses a similar tree layout
  and growth strategy.
