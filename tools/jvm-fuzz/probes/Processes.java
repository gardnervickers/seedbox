import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.TimeUnit;

/** Child processes: launch, pipes in both directions, exit codes, redirection, destroy and onExit. */
public final class Processes {
    public static void main(String[] args) throws Exception {
        Fz.init("Processes", args);
        int n = 20 * Fz.scale;
        long codes = 0;
        for (int i = 0; i < n; i++) {
            Process p = new ProcessBuilder("/bin/sh", "-c", "echo out$0; echo err$0 >&2; exit $(( $0 % 7 ))", Integer.toString(i)).start();
            String out = new String(p.getInputStream().readAllBytes(), StandardCharsets.UTF_8).trim();
            String err = new String(p.getErrorStream().readAllBytes(), StandardCharsets.UTF_8).trim();
            int code = p.waitFor();
            Fz.expect(out.equals("out" + i) && err.equals("err" + i), "pipes " + out + "/" + err);
            codes = codes * 31 + code;
        }
        Fz.out("codes", codes);
        byte[] payload = Fz.bytes(300_000);
        Process cat = new ProcessBuilder("/bin/cat").start();
        Thread writer = new Thread(() -> {
            try (OutputStream o = cat.getOutputStream()) { o.write(payload); } catch (IOException e) { throw new UncheckedIOException(e); }
        });
        writer.start();
        byte[] back = cat.getInputStream().readAllBytes();
        writer.join();
        Fz.out("cat", (cat.waitFor() == 0) + " " + java.util.Arrays.equals(back, payload));
        Path tmp = Files.createTempFile("fz", ".txt");
        Process redir = new ProcessBuilder("/bin/sh", "-c", "seq 1 1000").redirectOutput(tmp.toFile()).redirectErrorStream(true).start();
        redir.waitFor();
        Fz.out("redirect", Files.readAllLines(tmp).size());
        Files.delete(tmp);
        Process sleeper = new ProcessBuilder("/bin/sleep", "600").start();
        java.util.concurrent.CompletableFuture<Process> exited = sleeper.onExit();
        sleeper.destroy();
        Fz.out("destroy", exited.get(60, TimeUnit.SECONDS).exitValue() + " " + sleeper.isAlive());
        Process pipeline = ProcessBuilder.startPipeline(java.util.List.of(new ProcessBuilder("/bin/sh", "-c", "seq 1 5000"), new ProcessBuilder("/usr/bin/sort", "-r"), new ProcessBuilder("/usr/bin/head", "-3"))).get(2);
        Fz.out("pipeline", new String(pipeline.getInputStream().readAllBytes()).trim().replace('\n', ','));
        Fz.out("env", new String(new ProcessBuilder("/usr/bin/env").start().getInputStream().readAllBytes()).contains("PATH="));
        // Exec chains of non-JVM processes, without the JVM's own spawn path in between.
        Process deep = new ProcessBuilder("/bin/sh", "-c", "sh -c 'sh -c \"sh -c \\\"echo deep\\\"\"'").start();
        Fz.out("deep", new String(deep.getInputStream().readAllBytes()).trim() + " " + deep.waitFor());
        Fz.done();
    }
}
