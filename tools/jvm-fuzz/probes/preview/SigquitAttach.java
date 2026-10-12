import java.io.ByteArrayOutputStream;
import java.net.StandardProtocolFamily;
import java.net.UnixDomainSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * HotSpot's attach protocol done by hand, without a child process or jdk.attach: create the
 * attach file, send SIGQUIT (kill through the foreign-function API; Signal.raise refuses QUIT)
 * so the signal dispatcher starts the attach listener, then send
 * diagnostic commands over its Unix socket (thread dump, GC, heap walk, code cache, native heap
 * trim, heap dump). Only return codes and whether output arrived are printed.
 */
public final class SigquitAttach {
    static final String[] COMMANDS = {
        "Thread.print", "GC.run", "GC.class_histogram", "Compiler.codecache", "Compiler.queue",
        "VM.classloader_stats", "System.trim_native_heap", "VM.uptime", "GC.heap_info",
    };

    static String send(Path socket, String command) throws Exception {
        try (SocketChannel ch = SocketChannel.open(StandardProtocolFamily.UNIX)) {
            ch.connect(UnixDomainSocketAddress.of(socket));
            ByteArrayOutputStream req = new ByteArrayOutputStream();
            for (String part : new String[] {"1", "jcmd", command, "", ""}) {
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

    public static void main(String[] args) throws Exception {
        Fz.init("SigquitAttach", args);
        long pid = ProcessHandle.current().pid();
        Path socket = Path.of("/tmp/.java_pid" + pid);
        Path trigger = Path.of("/tmp/.attach_pid" + pid);
        Files.deleteIfExists(trigger);
        Files.createFile(trigger);
        Fz.expect(Native.killSelf(Native.SIGQUIT), "kill SIGQUIT");
        for (int i = 0; i < 3000 && !Files.exists(socket); i++) Thread.sleep(10);
        Fz.expect(Files.exists(socket), "attach listener started");
        Files.deleteIfExists(trigger);
        Path dump = Path.of(System.getProperty("java.io.tmpdir"), "fz-" + pid + ".hprof");
        String[] commands = java.util.Arrays.copyOf(COMMANDS, COMMANDS.length + 1);
        commands[COMMANDS.length] = "GC.heap_dump " + dump;
        for (String command : commands) {
            String r = send(socket, command);
            int nl = r.indexOf('\n');
            String code = nl < 0 ? r : r.substring(0, nl);
            Fz.out("cmd " + command.split(" ")[0], "rc=" + code + " output=" + (r.length() > nl + 1));
        }
        Fz.out("heapDumpWritten", Files.size(dump) > 1_000_000);
        Files.delete(dump);
        Fz.done();
    }
}
