/**
 * Web Worker for solving a ChaCha8 proof of work.
 *
 * The server defines a ChaCha8 keystream by providing a 64-digit hex key. This is the `nonce` that
 * defines the challenge. This worker then generates that keystream and searches the output for
 * 32-bit words having at least `difficulty` leading zero bits. The goal is to find 10 word offsets
 * in the keystream where this is the case.
 *
 * Receives:
 * - To start searching: `{ nonce: "64-digit hex", difficulty: N }`
 *
 * Sends:
 *  - To report partial progress: `{ type: "progress", found: COUNT }`
 *  - To report a complete solution: `{ type: "done", offsets: [...] }`
 */
self.onmessage = function(msg) {
    const difficulty = msg.data.difficulty;
    const num_offsets = 10;
    const offsets = []; // word offsets into the stream

    // Build the initial ChaCha8 input block with the nonce in the key position
    const input = buildInputBlock(msg.data.nonce);

    // Search the keystream by mutating `input` and repeatedly running ChaCha8
    for (let counter = 0; true; counter++) {
        input[12] = counter; // increment the least-significant counter word
        const output = chacha8Block(input);
        for (let i = 0; i < 16; i++) {
            if (Math.clz32(output[i]) >= difficulty) {
                offsets.push(counter * 16 + i);
                if (offsets.length == num_offsets) {
                    self.postMessage({ type: "done", offsets: offsets });
                    return;
                } else {
                    self.postMessage({ type: "progress", found: offsets.length });
                }
            }
        }
    }
};

// Build a 16-word ChaCha8 input block with the given hex string in the key position.
function buildInputBlock(hexKey) {
    const block = new Uint32Array(16);

    // Words 0-3: "expand 32-byte k"
    block[0] = 0x61707865;
    block[1] = 0x3320646e;
    block[2] = 0x79622d32;
    block[3] = 0x6b206574;

    // Words 4-11: hex digits as little-endian u32 words
    const keyBytes = new Uint8Array(32).map(
        (_, i) => parseInt(hexKey.substring(i * 2, i * 2 + 2), 16)
    );
    for (let i = 0; i < 8; i++) {
        const keyOffset = i * 4;
        const k = keyBytes.slice(keyOffset, keyOffset + 4);
        block[i + 4] = (k[3] << 24) | (k[2] << 16) | (k[1] << 8) | k[0];
    }

    // Words 12-13 (counter) and 14-15 (nonce) are left as zero
    return block;
}

// Run the ChaCha8 block operation.
//
// Takes a 16-word input. Returns a new 16-word output without mutating the input.
function chacha8Block(input) {
    const s = new Uint32Array(input);

    // Each loop computes 2 rounds, for a total of 8
    for (let i = 0; i < 4; i++) {
        // Even round: permute columns. The four quarter-round operations are interleaved for
        // performance, mixing words: (0,4,8,12), (1,5,9,13), (2,6,10,14), (3,7,11,15).
        s[0]  += s[4];
        s[1]  += s[5];
        s[2]  += s[6];
        s[3]  += s[7];

        s[12] = (s[12] ^ s[0])  << 16 | (s[12] ^ s[0])  >>> 16;
        s[13] = (s[13] ^ s[1])  << 16 | (s[13] ^ s[1])  >>> 16;
        s[14] = (s[14] ^ s[2])  << 16 | (s[14] ^ s[2])  >>> 16;
        s[15] = (s[15] ^ s[3])  << 16 | (s[15] ^ s[3])  >>> 16;

        s[8]  += s[12];
        s[9]  += s[13];
        s[10] += s[14];
        s[11] += s[15];

        s[4]  = (s[4]  ^ s[8])  << 12 | (s[4]  ^ s[8])  >>> 20;
        s[5]  = (s[5]  ^ s[9])  << 12 | (s[5]  ^ s[9])  >>> 20;
        s[6]  = (s[6]  ^ s[10]) << 12 | (s[6]  ^ s[10]) >>> 20;
        s[7]  = (s[7]  ^ s[11]) << 12 | (s[7]  ^ s[11]) >>> 20;

        s[0]  += s[4];
        s[1]  += s[5];
        s[2]  += s[6];
        s[3]  += s[7];

        s[12] = (s[12] ^ s[0])  <<  8 | (s[12] ^ s[0])  >>> 24;
        s[13] = (s[13] ^ s[1])  <<  8 | (s[13] ^ s[1])  >>> 24;
        s[14] = (s[14] ^ s[2])  <<  8 | (s[14] ^ s[2])  >>> 24;
        s[15] = (s[15] ^ s[3])  <<  8 | (s[15] ^ s[3])  >>> 24;

        s[8]  += s[12];
        s[9]  += s[13];
        s[10] += s[14];
        s[11] += s[15];

        s[4]  = (s[4]  ^ s[8])  <<  7 | (s[4]  ^ s[8])  >>> 25;
        s[5]  = (s[5]  ^ s[9])  <<  7 | (s[5]  ^ s[9])  >>> 25;
        s[6]  = (s[6]  ^ s[10]) <<  7 | (s[6]  ^ s[10]) >>> 25;
        s[7]  = (s[7]  ^ s[11]) <<  7 | (s[7]  ^ s[11]) >>> 25;

        // Odd round: permute diagonals. Quarter-rounds are again interleaved, mixing words:
        // (0,5,10,15), (1,6,11,12), (2,7,8,13), (3,4,9,14).
        s[0]  += s[5];
        s[1]  += s[6];
        s[2]  += s[7];
        s[3]  += s[4];

        s[15] = (s[15] ^ s[0])  << 16 | (s[15] ^ s[0])  >>> 16;
        s[12] = (s[12] ^ s[1])  << 16 | (s[12] ^ s[1])  >>> 16;
        s[13] = (s[13] ^ s[2])  << 16 | (s[13] ^ s[2])  >>> 16;
        s[14] = (s[14] ^ s[3])  << 16 | (s[14] ^ s[3])  >>> 16;

        s[10] += s[15];
        s[11] += s[12];
        s[8]  += s[13];
        s[9]  += s[14];

        s[5]  = (s[5]  ^ s[10]) << 12 | (s[5]  ^ s[10]) >>> 20;
        s[6]  = (s[6]  ^ s[11]) << 12 | (s[6]  ^ s[11]) >>> 20;
        s[7]  = (s[7]  ^ s[8])  << 12 | (s[7]  ^ s[8])  >>> 20;
        s[4]  = (s[4]  ^ s[9])  << 12 | (s[4]  ^ s[9])  >>> 20;

        s[0]  += s[5];
        s[1]  += s[6];
        s[2]  += s[7];
        s[3]  += s[4];

        s[15] = (s[15] ^ s[0])  <<  8 | (s[15] ^ s[0])  >>> 24;
        s[12] = (s[12] ^ s[1])  <<  8 | (s[12] ^ s[1])  >>> 24;
        s[13] = (s[13] ^ s[2])  <<  8 | (s[13] ^ s[2])  >>> 24;
        s[14] = (s[14] ^ s[3])  <<  8 | (s[14] ^ s[3])  >>> 24;

        s[10] += s[15];
        s[11] += s[12];
        s[8]  += s[13];
        s[9]  += s[14];

        s[5]  = (s[5]  ^ s[10]) <<  7 | (s[5]  ^ s[10]) >>> 25;
        s[6]  = (s[6]  ^ s[11]) <<  7 | (s[6]  ^ s[11]) >>> 25;
        s[7]  = (s[7]  ^ s[8])  <<  7 | (s[7]  ^ s[8])  >>> 25;
        s[4]  = (s[4]  ^ s[9])  <<  7 | (s[4]  ^ s[9])  >>> 25;
    }

    // Finalize: add back original state
    for (let i = 0; i < 16; i++) {
        s[i] += input[i];
    }
    return s;
}
