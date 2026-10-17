import java.math.BigInteger;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.MessageDigest;
import java.security.SecureRandom;
import java.security.Signature;
import java.util.zip.Adler32;
import java.util.zip.CRC32;
import java.util.zip.CRC32C;
import javax.crypto.Cipher;
import javax.crypto.Mac;
import javax.crypto.spec.ChaCha20ParameterSpec;
import javax.crypto.spec.GCMParameterSpec;
import javax.crypto.spec.IvParameterSpec;
import javax.crypto.spec.SecretKeySpec;

/** Digest, cipher, MAC, checksum and BigInteger intrinsics (AES, SHA, PMULL, CRC32 and Montgomery code). */
public final class Crypto {
    public static void main(String[] args) throws Exception {
        Fz.init("Crypto", args);
        byte[] data = Fz.bytes(64 * 1024 + Fz.rnd.nextInt(1024));
        String[] digests = {"MD5", "SHA-1", "SHA-224", "SHA-256", "SHA-384", "SHA-512", "SHA3-256", "SHA3-512"};
        for (int r = 0; r < 20 * Fz.scale; r++) {
            for (String d : digests) {
                byte[] h = MessageDigest.getInstance(d).digest(data);
                Fz.mix(h);
                if (r == 0) Fz.out(d, new BigInteger(1, h).toString(16));
            }
        }
        byte[] key = Fz.bytes(32), iv = Fz.bytes(16), nonce = Fz.bytes(12);
        String[][] ciphers = {
            {"AES/CBC/PKCS5Padding", "AES"}, {"AES/CTR/NoPadding", "AES"}, {"AES/ECB/PKCS5Padding", "AES"},
            {"AES/GCM/NoPadding", "AES"}, {"ChaCha20-Poly1305", "ChaCha20"}, {"ChaCha20", "ChaCha20"}};
        for (String[] c : ciphers) {
            Cipher enc = Cipher.getInstance(c[0]);
            Cipher dec = Cipher.getInstance(c[0]);
            SecretKeySpec k = new SecretKeySpec(key, c[1]);
            byte[] ct = null;
            for (int r = 0; r < 10 * Fz.scale; r++) {
                // GCM and ChaCha20 refuse a repeated key/nonce pair, so each round uses its own nonce.
                byte[] n12 = nonce.clone();
                n12[0] = (byte) r;
                byte[] n16 = iv.clone();
                n16[0] = (byte) r;
                java.security.spec.AlgorithmParameterSpec p = c[0].startsWith("AES/GCM") ? new GCMParameterSpec(128, n12)
                    : c[0].equals("ChaCha20") ? new ChaCha20ParameterSpec(n12, 7)
                    : c[0].startsWith("ChaCha20") ? new IvParameterSpec(n12)
                    : c[0].contains("ECB") ? null : new IvParameterSpec(n16);
                if (p == null) { enc.init(Cipher.ENCRYPT_MODE, k); dec.init(Cipher.DECRYPT_MODE, k); }
                else { enc.init(Cipher.ENCRYPT_MODE, k, p); dec.init(Cipher.DECRYPT_MODE, k, p); }
                ct = enc.doFinal(data);
                Fz.expect(java.util.Arrays.equals(dec.doFinal(ct), data), c[0] + " roundtrip");
            }
            Fz.out(c[0], Integer.toHexString(java.util.Arrays.hashCode(ct)) + " " + ct.length);
        }
        for (String m : new String[] {"HmacSHA1", "HmacSHA256", "HmacSHA512"}) {
            Mac mac = Mac.getInstance(m);
            mac.init(new SecretKeySpec(key, m));
            Fz.out(m, new BigInteger(1, mac.doFinal(data)).toString(16));
        }
        CRC32 crc = new CRC32(); CRC32C crcc = new CRC32C(); Adler32 ad = new Adler32();
        for (int off = 0; off < 4096; off += 1 + off / 3) { crc.update(data, off, data.length - off); crcc.update(data, off, 1000); ad.update(data, off, 777); }
        Fz.out("checksums", Long.toHexString(crc.getValue()) + " " + Long.toHexString(crcc.getValue()) + " " + Long.toHexString(ad.getValue()));
        SecureRandom sr = SecureRandom.getInstance("SHA1PRNG");
        sr.setSeed(Fz.seed);
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
        kpg.initialize(1024, sr);
        KeyPair kp = kpg.generateKeyPair();
        Signature sig = Signature.getInstance("SHA256withRSA");
        sig.initSign(kp.getPrivate());
        sig.update(data);
        byte[] signature = sig.sign();
        sig.initVerify(kp.getPublic());
        sig.update(data);
        Fz.out("rsa", Integer.toHexString(java.util.Arrays.hashCode(signature)) + " " + sig.verify(signature));
        BigInteger m = BigInteger.probablePrime(512, new java.util.Random(Fz.seed));
        BigInteger b = new BigInteger(1, Fz.bytes(64));
        BigInteger e = new BigInteger(1, Fz.bytes(48));
        Fz.outHex("modPow", b.modPow(e, m).hashCode());
        Fz.outHex("mulSquare", b.multiply(b.pow(30)).add(b.shiftLeft(77)).hashCode() ^ b.sqrt().hashCode() ^ b.modInverse(m).hashCode());
        Fz.done();
    }
}
