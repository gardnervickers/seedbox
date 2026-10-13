import java.io.ByteArrayOutputStream;
import java.io.File;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.ValueLayout;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.invoke.MethodHandle;
import java.net.StandardProtocolFamily;
import java.net.UnixDomainSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.ProtectionDomain;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.jar.Attributes;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;

/**
 * Late attach, as jcmd and VirtualMachine.loadAgent do it but by hand (the runtime image has no
 * jdk.attach): the attach file and SIGQUIT start the attach listener lazily, then the listener
 * runs "properties", "threaddump", "jcmd" and "load instrument", which calls agentmain on the
 * attach thread. Done once to this JVM and once from this JVM to a child JVM it starts. The agent
 * jar is written at run time and only carries a manifest; Agent-Class is already on the class
 * path. Run with -XX:+EnableDynamicAgentLoading.
 */
public final class AttachLoad {
    static final int SIGQUIT = 3;
    static final MethodHandle KILL;
    static {
        Linker l = Linker.nativeLinker();
        KILL = l.downcallHandle(l.defaultLookup().find("kill").orElseThrow(),
            FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
    }

    /** The dynamically loaded agent. */
    public static final class Agent {
        static volatile Instrumentation inst;

        public static void agentmain(String options, Instrumentation i) {
            inst = i;
            String prev = System.getProperty("fz.agent");
            System.setProperty("fz.agent", prev == null ? options : prev + "," + options);
        }
    }

    /** A class the attached agent retransforms. */
    static final class Victim {
        static int f(int x) { return x * 3 + 1; }
    }

    /** The child JVM: waits for the parent to attach, then reports what its agent saw. */
    public static final class Child {
        public static void main(String[] args) throws Exception {
            Path ready = Path.of(args[0]), stop = Path.of(args[1]);
            Files.writeString(ready, "");
            for (int i = 0; i < 60_000 && !Files.exists(stop); i++) Thread.sleep(2);
            System.out.println("child agent=" + System.getProperty("fz.agent") + " inst=" + (Agent.inst != null));
        }
    }

    static String send(Path socket, String... command) throws Exception {
        try (SocketChannel ch = SocketChannel.open(StandardProtocolFamily.UNIX)) {
            ch.connect(UnixDomainSocketAddress.of(socket));
            ByteArrayOutputStream req = new ByteArrayOutputStream();
            List<String> parts = new ArrayList<>(List.of("1"));
            parts.addAll(List.of(command));
            while (parts.size() < 5) parts.add("");
            for (String part : parts) {
                req.write(part.getBytes(StandardCharsets.UTF_8));
                req.write(0);
            }
            ch.write(ByteBuffer.wrap(req.toByteArray()));
            ByteArrayOutputStream resp = new ByteArrayOutputStream();
            ByteBuffer buf = ByteBuffer.allocate(65536);
            while (ch.read(buf) >= 0) {
                resp.write(buf.array(), 0, buf.position());
                buf.clear();
            }
            return resp.toString(StandardCharsets.UTF_8);
        }
    }

    static String status(String response) {
        int nl = response.indexOf('\n');
        return nl < 0 ? response : response.substring(0, nl);
    }

    /** A load response: status and the "return code" line, which name no paths. */
    static String loadResult(String response) {
        List<String> lines = response.lines().toList();
        String rc = lines.stream().filter(l -> l.startsWith("return code: ")).findFirst().orElse("no-return-code");
        return "status=" + (lines.isEmpty() ? "none" : lines.get(0)) + " " + rc;
    }

    /** Creates the attach trigger in `pid`'s working directory and sends SIGQUIT; returns the socket. */
    static Path startListener(long pid, Path cwd) throws Throwable {
        Path socket = Path.of("/tmp/.java_pid" + pid);
        Path trigger = cwd.resolve(".attach_pid" + pid);
        Files.deleteIfExists(trigger);
        Files.createFile(trigger);
        Fz.expect((int) KILL.invokeExact((int) pid, SIGQUIT) == 0, "kill SIGQUIT");
        for (int i = 0; i < 6000 && !Files.exists(socket); i++) Thread.sleep(5);
        Files.deleteIfExists(trigger);
        Fz.expect(Files.exists(socket), "attach listener started for " + (pid == ProcessHandle.current().pid() ? "self" : "child"));
        return socket;
    }

    static Path agentJar(Path dir) throws Exception {
        Path jar = dir.resolve("fz-agent.jar");
        Manifest m = new Manifest();
        m.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        m.getMainAttributes().putValue("Agent-Class", "AttachLoad$Agent");
        m.getMainAttributes().putValue("Can-Retransform-Classes", "true");
        try (JarOutputStream out = new JarOutputStream(Files.newOutputStream(jar), m)) {
            out.flush();
        }
        return jar;
    }

    static void self(Path jar) throws Throwable {
        long pid = ProcessHandle.current().pid();
        System.setProperty("fz.marker", "m" + Fz.seed);
        Path socket = startListener(pid, Path.of("").toAbsolutePath());
        String props = send(socket, "properties");
        Fz.out("self properties", "status=" + status(props) + " marker=" + props.contains("fz.marker=m" + Fz.seed));
        Fz.out("self agentProperties", "status=" + status(send(socket, "agentProperties")));
        String dump = send(socket, "threaddump");
        Fz.out("self threaddump", "status=" + status(dump) + " full=" + dump.contains("Full thread dump"));
        int loads = 2 + Fz.rnd.nextInt(3);
        for (int i = 0; i < loads; i++) {
            Fz.out("self load" + i, loadResult(send(socket, "load", "instrument", "false", jar + "=self" + i)));
        }
        StringBuilder want = new StringBuilder();
        for (int i = 0; i < loads; i++) want.append(i == 0 ? "" : ",").append("self").append(i);
        Fz.out("self agentmain", System.getProperty("fz.agent").equals(want.toString()));
        Fz.expect(System.getProperty("fz.agent").equals(want.toString()) && Agent.inst != null, "agentmain ran for every load");
        Fz.out("self missingJar", loadResult(send(socket, "load", "instrument", "false", jar + ".missing=x")));
        Fz.out("self jcmd", status(send(socket, "jcmd", "VM.version")));
        Instrumentation inst = Agent.inst;
        AtomicInteger seen = new AtomicInteger();
        ClassFileTransformer t = new ClassFileTransformer() {
            @Override
            public byte[] transform(Module m, ClassLoader l, String name, Class<?> c, ProtectionDomain pd, byte[] b) {
                if ("AttachLoad$Victim".equals(name)) seen.incrementAndGet();
                return null;
            }
        };
        int before = Victim.f(7);
        inst.addTransformer(t, true);
        inst.retransformClasses(Victim.class);
        inst.removeTransformer(t);
        Fz.out("self retransform", seen.get() + " " + inst.isRetransformClassesSupported() + " " + (Victim.f(7) == before));
        Fz.mix(loads);
    }

    static void child(Path dir, Path jar) throws Throwable {
        Path ready = dir.resolve("ready"), stop = dir.resolve("stop");
        List<String> cmd = List.of(Path.of(System.getProperty("java.home"), "bin", "java").toString(),
            "-Xmx64m", "-Xss1m", "-XX:-UsePerfData", "-XX:TieredStopAtLevel=1", "-XX:+EnableDynamicAgentLoading", "--enable-preview",
            "-cp", System.getProperty("java.class.path"), "AttachLoad$Child", ready.toString(), stop.toString());
        Process p = new ProcessBuilder(cmd).directory(dir.toFile()).redirectErrorStream(true)
            .redirectInput(ProcessBuilder.Redirect.from(new File("/dev/null"))).start();
        for (int i = 0; i < 12_000 && !Files.exists(ready) && p.isAlive(); i++) Thread.sleep(5);
        Fz.expect(Files.exists(ready), "child started");
        Path socket = startListener(p.pid(), dir);
        Fz.out("child jcmd", status(send(socket, "jcmd", "VM.version")));
        Fz.out("child load", loadResult(send(socket, "load", "instrument", "false", jar + "=child" + Fz.seed)));
        String dump = send(socket, "jcmd", "Thread.print");
        Fz.out("child threadPrint", "status=" + status(dump) + " main=" + dump.contains("\"main\""));
        Files.writeString(stop, "");
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        p.getInputStream().transferTo(out);
        int code = p.waitFor();
        for (String line : out.toString(StandardCharsets.UTF_8).lines().toList()) Fz.out("child out", line);
        Fz.out("child exit", code);
        Fz.expect(code == 0 && out.toString(StandardCharsets.UTF_8).contains("child agent=child" + Fz.seed + " inst=true"), "child agentmain");
        Files.deleteIfExists(Path.of("/tmp/.java_pid" + p.pid()));
    }

    public static void main(String[] args) throws Throwable {
        Fz.init("AttachLoad", args);
        Path dir = Path.of(System.getProperty("java.io.tmpdir"), "fzattach");
        Files.createDirectories(dir);
        Path jar = agentJar(dir);
        self(jar);
        child(dir, jar);
        try (var files = Files.list(dir)) {
            for (Path f : files.toList()) Files.delete(f);
        }
        Files.delete(dir);
        Fz.done();
    }
}
