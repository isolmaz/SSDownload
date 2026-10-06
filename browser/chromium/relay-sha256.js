"use strict";

// Incremental SHA-256 with a small serializable state.  WebCrypto's digest API
// requires the entire input, so it cannot be used for multi-gigabyte relays.
(function installRelaySha256(root) {
  const K = new Uint32Array([
    0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
    0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
    0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
    0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
    0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,
    0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
    0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,
    0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2
  ]);
  const INITIAL = [0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19];
  const rotate = (value, bits) => (value >>> bits) | (value << (32 - bits));
  const hex = value => value.toString(16).padStart(8, "0");

  class RelaySha256 {
    constructor(saved) {
      this.h = new Uint32Array(INITIAL);
      this.buffer = new Uint8Array(64);
      this.bufferLength = 0;
      this.bytes = 0;
      this.words = new Uint32Array(64);
      if (saved !== undefined) this.restore(saved);
    }

    restore(saved) {
      if (!saved || saved.v !== 1 || !Array.isArray(saved.h) || saved.h.length !== 8
          || !saved.h.every(Number.isInteger) || !Number.isSafeInteger(saved.bytes) || saved.bytes < 0
          || typeof saved.tail !== "string" || saved.tail.length > 88) throw new Error("Geçersiz SHA-256 sürdürme kaydı");
      const binary = atob(saved.tail);
      if (binary.length >= 64 || binary.length !== saved.bytes % 64) throw new Error("SHA-256 sürdürme kuyruğu geçersiz");
      this.h.set(saved.h.map(value => value >>> 0));
      this.bytes = saved.bytes;
      this.bufferLength = binary.length;
      for (let index = 0; index < binary.length; index++) this.buffer[index] = binary.charCodeAt(index);
      return this;
    }

    exportState() {
      let tail = "";
      for (let index = 0; index < this.bufferLength; index++) tail += String.fromCharCode(this.buffer[index]);
      return { v: 1, h: Array.from(this.h), bytes: this.bytes, tail: btoa(tail) };
    }

    clone() { return new RelaySha256(this.exportState()); }

    update(input) {
      const bytes = input instanceof Uint8Array ? input : new Uint8Array(input);
      if (!Number.isSafeInteger(this.bytes + bytes.length)) throw new Error("SHA-256 uzunluğu desteklenen sınırı aştı");
      this.bytes += bytes.length;
      let offset = 0;
      if (this.bufferLength) {
        const count = Math.min(64 - this.bufferLength, bytes.length);
        this.buffer.set(bytes.subarray(0, count), this.bufferLength);
        this.bufferLength += count;
        offset = count;
        if (this.bufferLength === 64) { this.compress(this.buffer); this.bufferLength = 0; }
      }
      while (offset + 64 <= bytes.length) { this.compress(bytes.subarray(offset, offset + 64)); offset += 64; }
      if (offset < bytes.length) {
        this.buffer.set(bytes.subarray(offset), 0);
        this.bufferLength = bytes.length - offset;
      }
      return this;
    }

    compress(block) {
      const w = this.words;
      for (let index = 0; index < 16; index++) {
        const offset = index * 4;
        w[index] = ((block[offset] << 24) | (block[offset + 1] << 16) | (block[offset + 2] << 8) | block[offset + 3]) >>> 0;
      }
      for (let index = 16; index < 64; index++) {
        const a = w[index - 15], b = w[index - 2];
        const s0 = rotate(a, 7) ^ rotate(a, 18) ^ (a >>> 3);
        const s1 = rotate(b, 17) ^ rotate(b, 19) ^ (b >>> 10);
        w[index] = (w[index - 16] + s0 + w[index - 7] + s1) >>> 0;
      }
      let [a,b,c,d,e,f,g,h] = this.h;
      for (let index = 0; index < 64; index++) {
        const s1 = rotate(e, 6) ^ rotate(e, 11) ^ rotate(e, 25);
        const choice = (e & f) ^ (~e & g);
        const t1 = (h + s1 + choice + K[index] + w[index]) >>> 0;
        const s0 = rotate(a, 2) ^ rotate(a, 13) ^ rotate(a, 22);
        const majority = (a & b) ^ (a & c) ^ (b & c);
        const t2 = (s0 + majority) >>> 0;
        h=g; g=f; f=e; e=(d+t1)>>>0; d=c; c=b; b=a; a=(t1+t2)>>>0;
      }
      this.h[0]=(this.h[0]+a)>>>0; this.h[1]=(this.h[1]+b)>>>0;
      this.h[2]=(this.h[2]+c)>>>0; this.h[3]=(this.h[3]+d)>>>0;
      this.h[4]=(this.h[4]+e)>>>0; this.h[5]=(this.h[5]+f)>>>0;
      this.h[6]=(this.h[6]+g)>>>0; this.h[7]=(this.h[7]+h)>>>0;
    }

    digestHex() {
      const copy = this.clone();
      const bitLength = copy.bytes * 8;
      const padding = new Uint8Array(copy.bufferLength < 56 ? 64 - copy.bufferLength : 128 - copy.bufferLength);
      padding[0] = 0x80;
      const high = Math.floor(bitLength / 0x100000000);
      const low = bitLength >>> 0;
      const end = padding.length;
      padding[end - 8]=(high>>>24)&255; padding[end - 7]=(high>>>16)&255; padding[end - 6]=(high>>>8)&255; padding[end - 5]=high&255;
      padding[end - 4]=(low>>>24)&255; padding[end - 3]=(low>>>16)&255; padding[end - 2]=(low>>>8)&255; padding[end - 1]=low&255;
      copy.update(padding);
      return Array.from(copy.h, hex).join("");
    }

    static digestHex(bytes) { return new RelaySha256().update(bytes).digestHex(); }
  }

  root.SSDownloadRelaySha256 = RelaySha256;
})(globalThis);
