import java.math.BigInteger;
import java.security.KeyFactory;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.PrivateKey;
import java.security.PublicKey;
import java.security.SecureRandom;
import java.security.Signature;
import java.security.spec.ECGenParameterSpec;
import java.security.spec.EdDSAParameterSpec;
import java.security.spec.EdECPrivateKeySpec;
import java.security.spec.NamedParameterSpec;
import java.security.spec.PKCS8EncodedKeySpec;
import java.security.spec.X509EncodedKeySpec;
import java.util.Arrays;
import javax.crypto.KeyAgreement;

/**
 * Edwards and Montgomery curve arithmetic: Ed25519 and Ed448 keys from a seeded SHA1PRNG and
 * from raw private scalars, deterministic signatures (also with Ed25519ctx/Ed448 contexts),
 * verification of tampered messages and signatures, key encoding round trips, X25519/X448 key
 * agreement, and ECDSA P-256/P-384 with a seeded nonce source. All results are fixed by the
 * seed and printed as hashes.
 */
public final class EdCurves {
    static SecureRandom seeded(String salt) throws Exception {
        SecureRandom r = SecureRandom.getInstance("SHA1PRNG");
        r.setSeed((salt + Fz.seed).getBytes());
        return r;
    }

    static long hash(byte[] b) { return Arrays.hashCode(b) & 0xffffffffL; }

    static void eddsa(String curve, NamedParameterSpec spec, int rawLen) throws Exception {
        KeyPairGenerator g = KeyPairGenerator.getInstance(curve);
        g.initialize(spec, seeded(curve));
        KeyPair kp = g.generateKeyPair();
        byte[] raw = new byte[rawLen];
        seeded(curve + "raw").nextBytes(raw);
        KeyFactory kf = KeyFactory.getInstance(curve);
        PrivateKey fromRaw = kf.generatePrivate(new EdECPrivateKeySpec(spec, raw));
        // Encoding round trips.
        PublicKey pub = kf.generatePublic(new X509EncodedKeySpec(kp.getPublic().getEncoded()));
        PrivateKey priv = kf.generatePrivate(new PKCS8EncodedKeySpec(kp.getPrivate().getEncoded()));
        long sigs = 0;
        int verified = 0, rejected = 0, n = 40 * Fz.scale;
        Signature s = Signature.getInstance(curve);
        for (int i = 0; i < n; i++) {
            byte[] msg = Fz.bytes(Fz.rnd.nextInt(i % 5 == 0 ? 5000 : 64));
            boolean ctx = i % 3 == 1;
            byte[] context = ctx ? Fz.bytes(1 + Fz.rnd.nextInt(32)) : null;
            s.initSign(i % 2 == 0 ? priv : fromRaw);
            if (ctx) s.setParameter(new EdDSAParameterSpec(false, context));
            s.update(msg);
            byte[] sig = s.sign();
            // Signing is deterministic: the same input signs identically.
            s.initSign(i % 2 == 0 ? priv : fromRaw);
            if (ctx) s.setParameter(new EdDSAParameterSpec(false, context));
            s.update(msg);
            Fz.expect(Arrays.equals(sig, s.sign()), "deterministic signature");
            sigs = sigs * 31 + hash(sig);
            if (i % 2 == 0) {
                s.initVerify(pub);
                if (ctx) s.setParameter(new EdDSAParameterSpec(false, context));
                s.update(msg);
                if (s.verify(sig)) verified++;
                byte[] bad = sig.clone();
                bad[Fz.rnd.nextInt(bad.length)] ^= (byte) (1 << Fz.rnd.nextInt(8));
                s.initVerify(pub);
                if (ctx) s.setParameter(new EdDSAParameterSpec(false, context));
                s.update(msg);
                try {
                    if (!s.verify(bad)) rejected++;
                } catch (java.security.SignatureException e) {
                    rejected++;
                }
            }
        }
        Fz.out(curve + ".publicKey", Long.toHexString(hash(kp.getPublic().getEncoded())));
        Fz.out(curve + ".signatures", Long.toHexString(sigs));
        Fz.out(curve + ".verified", verified + "/" + (n + 1) / 2);
        Fz.out(curve + ".rejected", rejected + "/" + (n + 1) / 2);
        Fz.mix(sigs);
    }

    static void xdh(String curve) throws Exception {
        KeyPairGenerator g = KeyPairGenerator.getInstance(curve);
        g.initialize(new NamedParameterSpec(curve), seeded(curve));
        long h = 0;
        for (int i = 0; i < 20 * Fz.scale; i++) {
            KeyPair a = g.generateKeyPair(), b = g.generateKeyPair();
            KeyAgreement ka = KeyAgreement.getInstance(curve), kb = KeyAgreement.getInstance(curve);
            ka.init(a.getPrivate());
            ka.doPhase(b.getPublic(), true);
            kb.init(b.getPrivate());
            kb.doPhase(a.getPublic(), true);
            byte[] sa = ka.generateSecret(), sb = kb.generateSecret();
            Fz.expect(Arrays.equals(sa, sb), "shared secret");
            h = h * 31 + hash(sa);
        }
        Fz.out(curve + ".secrets", Long.toHexString(h));
        Fz.mix(h);
    }

    static void ecdsa(String curve) throws Exception {
        KeyPairGenerator g = KeyPairGenerator.getInstance("EC");
        g.initialize(new ECGenParameterSpec(curve), seeded(curve));
        KeyPair kp = g.generateKeyPair();
        Signature s = Signature.getInstance("SHA256withECDSA");
        long h = 0;
        int ok = 0, n = 20 * Fz.scale;
        SecureRandom nonces = seeded(curve + "k");
        for (int i = 0; i < n; i++) {
            byte[] msg = Fz.bytes(1 + Fz.rnd.nextInt(300));
            s.initSign(kp.getPrivate(), nonces);
            s.update(msg);
            byte[] sig = s.sign();
            s.initVerify(kp.getPublic());
            s.update(msg);
            if (s.verify(sig)) ok++;
            h = h * 31 + hash(sig);
        }
        Fz.out("ecdsa." + curve, Long.toHexString(h) + " " + ok + "/" + n);
        Fz.mix(h);
        Fz.mix(new BigInteger(kp.getPublic().getEncoded()).bitLength());
    }

    public static void main(String[] args) throws Exception {
        Fz.init("EdCurves", args);
        eddsa("Ed25519", NamedParameterSpec.ED25519, 32);
        eddsa("Ed448", NamedParameterSpec.ED448, 57);
        xdh("X25519");
        xdh("X448");
        ecdsa("secp256r1");
        ecdsa("secp384r1");
        Fz.done();
    }
}
