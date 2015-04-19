import hashlib
import mmh3


def consistent_hash(key, num_buckets):
    """
    From: https://github.com/snormore/pyjumphash/pull/1/files

    A Fast, Minimal Memory, Consistent Hash Algorithm (Jump Consistent Hash)
    Hash accepts "a 64-bit key and the number of buckets. It outputs a number
    in the range [0, buckets]." - http://arxiv.org/ftp/arxiv/papers/1406/1406.2294.pdf
    The C++ implementation they provide is as follows:
    int32_t JumpConsistentHash(uint64_t key, int32_t num_buckets) {
        int64_t b = -1, j = 0;
        while (j < num_buckets) {
            b   = j;
            key = key * 2862933555777941757ULL + 1;
            j   = (b + 1) * (double(1LL << 31) / double((key >> 33) + 1));
        }
        return b;
    }
    """
    if not isinstance(key, (int, long)):
        if isinstance(key, unicode):
            key = key.encode('utf-8')
        key = int(hashlib.md5(key).hexdigest(), 16) & 0xffffffffffffffff
    b, j = -1, 0
    if num_buckets < 0:
        num_buckets = 1
    while j < num_buckets:
        b = int(j)
        key = ((key * 2862933555777941757) + 1) & 0xffffffffffffffff
        j = float(b + 1) * (float(1 << 31) / float((key >> 33) + 1))
    return b & 0xffffffff

def bloom_hash(key, filter_size=128):
    from bitarray import bitarray

    bits = bitarray(filter_size)
    bits.setall(False)

    seeds = [42, 451, 33]
    for s in seeds:
        val = mmh3.hash(key, s)
        bit_pos = val % filter_size
        bits[bit_pos] = True

    return bits


if __name__ == '__main__':
    assert consistent_hash(1, 1) == 0
    assert consistent_hash(256, 1024) == 520
    assert consistent_hash(42, 57) == 43
    assert consistent_hash(0xDEAD10CC, -666) == 0
    assert consistent_hash(0xDEAD10CC, 1) == 0
    assert consistent_hash(0xDEAD10CC, 666) == 361
