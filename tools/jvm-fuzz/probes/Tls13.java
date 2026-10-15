import java.io.ByteArrayInputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.net.InetAddress;
import java.nio.ByteBuffer;
import java.security.KeyStore;
import java.security.MessageDigest;
import java.security.Security;
import java.util.ArrayList;
import java.util.Base64;
import java.util.List;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SNIHostName;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLEngineResult;
import javax.net.ssl.SSLParameters;
import javax.net.ssl.SSLServerSocket;
import javax.net.ssl.SSLSession;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.TrustManagerFactory;

/**
 * TLS 1.3 through JSSE over loopback with mutual authentication: a self-signed secp256r1
 * certificate (embedded PKCS12, CN and SAN localhost/127.0.0.1, valid 1970-2098 so any guest
 * epoch works), seeded cipher suites and key-exchange groups, a low key-usage limit that forces
 * KeyUpdate messages, several sequential connections, and an in-memory SSLEngine pair driven
 * with delegated tasks. Handshake randomness makes every transcript different, so only
 * application-level results and negotiated parameters are printed.
 */
public final class Tls13 {
    static final String P12 =
        "MIID+AIBAzCCA6IGCSqGSIb3DQEHAaCCA5MEggOPMIIDizCCASIGCSqGSIb3DQEHAaCCARMEggEPMIIBCzCCAQcGCyqGSIb3" +
        "DQEMCgECoIG9MIG6MGYGCSqGSIb3DQEFDTBZMDgGCSqGSIb3DQEFDDArBBTSe+YD5RUouSKPenFlcO8hS8fYsAICJxACASAw" +
        "DAYIKoZIhvcNAgkFADAdBglghkgBZQMEASoEEMcqzJnD3l31zp8BmmsjsRMEUAUM6s002OQt+cP/tGa24iKc05+5XkTsNtGg" +
        "O9UuGUDotoEyBkHH23YsoibGfKIKq+3UtecCEyMUawQwxhx3X9oxaf7CNBLbqwzUpaNVmikjMTgwEwYJKoZIhvcNAQkUMQYe" +
        "BABmAHowIQYJKoZIhvcNAQkVMRQEElRpbWUgMTc5MTg2NDA2MzE5OTCCAmEGCSqGSIb3DQEHBqCCAlIwggJOAgEAMIICRwYJ" +
        "KoZIhvcNAQcBMGYGCSqGSIb3DQEFDTBZMDgGCSqGSIb3DQEFDDArBBQTR4bAMHieEdIkkfSI52GjTnbJlwICJxACASAwDAYI" +
        "KoZIhvcNAgkFADAdBglghkgBZQMEASoEEHPc6SyXYeK+TST6Uim+0cCAggHQYH5zF6pphWTqfRsMgYpZgAjhA+ejjSwswqr7" +
        "2MoZPsDGxb85fJJo2RtQ0jvV+TTfu3s47N9NA0pfDpiJh/zRwqRmrB6suTFisaImG9b6LXuBYDfznM41tATwngaBxIbk5hBP" +
        "q1InGl8OPFPLP0DvTpLlIQn/EHSsw9F1C+H1ByYqmZE5f2PPN3LZL26R10RVVOrro9mQhQicvC6V5kOMN52oaLDMs5gEarhf" +
        "PZa8SPM6SfOWWriYs3ix4cxkam1s/wQbRxJOA7asOpwAfPxCJyGpzG6SbHzMj0QmfGFAOWUFK3DZ2alvm+kwZrNnN8MDmoIn" +
        "5eJiTmICvx8/YSVKBK0fWz3fSleB1wAAk0T4NFoDMqvUqNmEUnQjVfGbfxmyi6txKMFTppnTp2J8Z2HsQ6SkuZGGkXX+xDFZ" +
        "wCV8FqulqctW/PoK+qeH9bIhsP2HxXQSOcaBYyqLQcfuP9FuNL8XgPjr4ROAJo2hhwe1L8NPNifqt1/uNJYnQAevocquZ/Kv" +
        "H83dwrtgq5WNHoNkfSw+HMiJ0rq3WlwtsswLY4P834DIQd37HLWNSJKF2IAi8GKOBnwbgLbSDjkaFSAF+ybebpj6lSVQ4jKE" +
        "4h8AC+4wTTAxMA0GCWCGSAFlAwQCAQUABCBofsJ8pknXyyw8M/czD4FeqXp5Mh+FgfPvt2W6TlNecAQUpRITmP8P+0qe1tuU" +
        "0czNemQg014CAicQ";
    static final char[] PASS = "fzpass".toCharArray();
    static final String[] SUITES = {"TLS_AES_128_GCM_SHA256", "TLS_AES_256_GCM_SHA384", "TLS_CHACHA20_POLY1305_SHA256"};
    static final String[] GROUPS = {"x25519", "secp256r1", "secp384r1", "x448", "ffdhe2048"};

    static SSLContext context() throws Exception {
        KeyStore ks = KeyStore.getInstance("PKCS12");
        ks.load(new ByteArrayInputStream(Base64.getDecoder().decode(P12)), PASS);
        KeyManagerFactory kmf = KeyManagerFactory.getInstance("PKIX");
        kmf.init(ks, PASS);
        TrustManagerFactory tmf = TrustManagerFactory.getInstance("PKIX");
        tmf.init(ks);
        SSLContext c = SSLContext.getInstance("TLSv1.3");
        c.init(kmf.getKeyManagers(), tmf.getTrustManagers(), null);
        return c;
    }

    static byte[] transform(byte[] b) throws Exception {
        return MessageDigest.getInstance("SHA-256").digest(b);
    }

    static void sockets(SSLContext ctx) throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();
        int conns = 3 + Fz.rnd.nextInt(3);
        String[] suite = new String[conns], group = new String[conns];
        int[][] sizes = new int[conns][];
        for (int c = 0; c < conns; c++) {
            suite[c] = SUITES[Fz.rnd.nextInt(SUITES.length)];
            group[c] = GROUPS[Fz.rnd.nextInt(GROUPS.length)];
            sizes[c] = new int[4 + Fz.rnd.nextInt(8)];
            for (int k = 0; k < sizes[c].length; k++) sizes[c][k] = 1 + Fz.rnd.nextInt(Fz.rnd.nextInt(4) == 0 ? 60_000 : 2000);
        }
        List<String> serverSaw = new ArrayList<>();
        try (SSLServerSocket ss = (SSLServerSocket) ctx.getServerSocketFactory().createServerSocket(0, 4, lo)) {
            ss.setEnabledProtocols(new String[] {"TLSv1.3"});
            ss.setNeedClientAuth(true);
            Thread server = new Thread(() -> {
                try {
                    for (int c = 0; c < conns; c++) {
                        try (SSLSocket s = (SSLSocket) ss.accept()) {
                            s.setSoTimeout(60_000);
                            DataInputStream in = new DataInputStream(s.getInputStream());
                            DataOutputStream out = new DataOutputStream(s.getOutputStream());
                            long bytes = 0;
                            for (int n; (n = in.readInt()) >= 0; ) {
                                byte[] b = new byte[n];
                                in.readFully(b);
                                bytes += n;
                                byte[] r = transform(b);
                                out.writeInt(r.length + n);
                                out.write(r);
                                out.write(b);
                                out.flush();
                            }
                            SSLSession ses = s.getSession();
                            synchronized (serverSaw) {
                                serverSaw.add(ses.getProtocol() + "/" + ses.getCipherSuite() + "/"
                                    + ses.getPeerPrincipal().getName() + "/" + bytes);
                            }
                        }
                    }
                } catch (Exception e) {
                    synchronized (serverSaw) {
                        serverSaw.add("error " + e);
                    }
                }
            });
            server.start();
            long h = 0;
            int good = 0;
            for (int c = 0; c < conns; c++) {
                try (SSLSocket s = (SSLSocket) ctx.getSocketFactory().createSocket(lo, ss.getLocalPort())) {
                    s.setSoTimeout(60_000);
                    SSLParameters p = s.getSSLParameters();
                    p.setProtocols(new String[] {"TLSv1.3"});
                    p.setCipherSuites(new String[] {suite[c]});
                    p.setNamedGroups(new String[] {group[c]});
                    p.setServerNames(List.of(new SNIHostName("localhost")));
                    p.setEndpointIdentificationAlgorithm("HTTPS");
                    s.setSSLParameters(p);
                    s.startHandshake();
                    DataOutputStream out = new DataOutputStream(s.getOutputStream());
                    DataInputStream in = new DataInputStream(s.getInputStream());
                    boolean ok = true;
                    for (int n : sizes[c]) {
                        byte[] b = Fz.bytes(n);
                        out.writeInt(n);
                        out.write(b);
                        out.flush();
                        byte[] r = new byte[in.readInt()];
                        in.readFully(r);
                        byte[] want = transform(b);
                        ok &= java.util.Arrays.equals(java.util.Arrays.copyOf(r, want.length), want)
                            && java.util.Arrays.equals(java.util.Arrays.copyOfRange(r, want.length, r.length), b);
                        h = h * 31 + java.util.Arrays.hashCode(r);
                    }
                    out.writeInt(-1);
                    out.flush();
                    SSLSession ses = s.getSession();
                    Fz.out("conn" + c, ses.getProtocol() + " " + ses.getCipherSuite() + " " + group[c] + " "
                        + ses.getPeerPrincipal().getName() + " " + ok);
                    if (ok) good++;
                }
            }
            server.join(120_000);
            Fz.out("socket.good", good + "/" + conns);
            synchronized (serverSaw) {
                for (int c = 0; c < serverSaw.size(); c++) Fz.out("server" + c, serverSaw.get(c));
            }
            Fz.out("socket.dataHash", Long.toHexString(h));
            Fz.mix(h);
            Fz.expect(good == conns && serverSaw.size() == conns, "TLS connections");
        }
    }

    /** Runs one engine's handshake step; returns whether progress was made. */
    static boolean step(SSLEngine e, ByteBuffer app, ByteBuffer outNet, ByteBuffer inNet, ByteBuffer appIn, int[] tasks) throws Exception {
        boolean progress = false;
        SSLEngineResult r = e.wrap(app, outNet);
        progress |= r.bytesProduced() > 0 || r.bytesConsumed() > 0;
        Runnable t;
        while ((t = e.getDelegatedTask()) != null) { t.run(); tasks[0]++; progress = true; }
        inNet.flip();
        r = e.unwrap(inNet, appIn);
        inNet.compact();
        progress |= r.bytesProduced() > 0 || r.bytesConsumed() > 0;
        while ((t = e.getDelegatedTask()) != null) { t.run(); tasks[0]++; progress = true; }
        return progress;
    }

    static void engines(SSLContext ctx) throws Exception {
        SSLEngine client = ctx.createSSLEngine("localhost", 443), server = ctx.createSSLEngine();
        client.setUseClientMode(true);
        server.setUseClientMode(false);
        server.setNeedClientAuth(true);
        SSLParameters p = client.getSSLParameters();
        p.setProtocols(new String[] {"TLSv1.3"});
        p.setNamedGroups(new String[] {GROUPS[Fz.rnd.nextInt(GROUPS.length)]});
        p.setEndpointIdentificationAlgorithm("HTTPS");
        client.setSSLParameters(p);
        int cap = client.getSession().getPacketBufferSize() * 4, appCap = client.getSession().getApplicationBufferSize() * 4;
        byte[] cmsg = Fz.bytes(5000 + Fz.rnd.nextInt(50_000)), smsg = Fz.bytes(5000 + Fz.rnd.nextInt(50_000));
        ByteBuffer cApp = ByteBuffer.wrap(cmsg), sApp = ByteBuffer.wrap(smsg);
        ByteBuffer c2s = ByteBuffer.allocate(cap), s2c = ByteBuffer.allocate(cap);
        ByteBuffer cIn = ByteBuffer.allocate(appCap + smsg.length), sIn = ByteBuffer.allocate(appCap + cmsg.length);
        int[] tasks = {0};
        int idle = 0;
        for (int rounds = 0; rounds < 100_000 && idle < 50; rounds++) {
            boolean a = step(client, cApp, c2s, s2c, cIn, tasks);
            boolean b = step(server, sApp, s2c, c2s, sIn, tasks);
            boolean done = cIn.position() == smsg.length && sIn.position() == cmsg.length;
            if (done) break;
            idle = a || b ? 0 : idle + 1;
        }
        boolean ok = java.util.Arrays.equals(java.util.Arrays.copyOf(cIn.array(), cIn.position()), smsg)
            && java.util.Arrays.equals(java.util.Arrays.copyOf(sIn.array(), sIn.position()), cmsg);
        Fz.out("engine.exchange", ok + " " + client.getSession().getProtocol() + " " + server.getSession().getPeerPrincipal().getName());
        Fz.out("engine.delegatedTasks", tasks[0] > 0);
        Fz.expect(ok, "SSLEngine exchange");
        client.closeOutbound();
        for (int k = 0; k < 20 && !(client.isOutboundDone() && server.isInboundDone()); k++) {
            step(client, ByteBuffer.allocate(0), c2s, s2c, cIn, tasks);
            step(server, ByteBuffer.allocate(0), s2c, c2s, sIn, tasks);
        }
        Fz.out("engine.closed", client.isOutboundDone() + " " + server.isInboundDone());
        Fz.mix(cmsg);
        Fz.mix(smsg);
    }

    public static void main(String[] args) throws Exception {
        Fz.init("Tls13", args);
        // A low AES-GCM and ChaCha20 usage limit makes long streams send KeyUpdate messages.
        Security.setProperty("jdk.tls.keyLimits", "AES/GCM/NoPadding KeyUpdate 2^16, ChaCha20-Poly1305 KeyUpdate 2^16");
        SSLContext ctx = context();
        sockets(ctx);
        engines(ctx);
        Fz.done();
    }
}
