import com.sun.net.httpserver.HttpServer;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.Executors;

/** JDK HTTP server and asynchronous HTTP client over loopback. */
public final class Http {
    public static void main(String[] args) throws Exception {
        Fz.init("Http", args);
        HttpServer server = HttpServer.create(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0), 0);
        server.setExecutor(Executors.newFixedThreadPool(3));
        server.createContext("/echo", ex -> {
            byte[] body = ex.getRequestBody().readAllBytes();
            String q = ex.getRequestURI().getQuery();
            byte[] resp = (q + ":" + body.length + ":" + java.util.Arrays.hashCode(body)).getBytes();
            ex.getResponseHeaders().add("X-Len", Integer.toString(body.length));
            ex.sendResponseHeaders(200, resp.length);
            try (OutputStream o = ex.getResponseBody()) { o.write(resp); }
        });
        server.start();
        int port = server.getAddress().getPort();
        HttpClient client = HttpClient.newBuilder().version(HttpClient.Version.HTTP_1_1).build();
        int n = 100 * Fz.scale;
        long sync = 0;
        for (int i = 0; i < n; i++) {
            HttpRequest req = HttpRequest.newBuilder(URI.create("http://127.0.0.1:" + port + "/echo?i=" + i))
                .POST(HttpRequest.BodyPublishers.ofByteArray(new byte[i * 13 % 4000])).build();
            HttpResponse<String> r = client.send(req, HttpResponse.BodyHandlers.ofString());
            sync = sync * 31 + r.body().hashCode() + r.statusCode() + Integer.parseInt(r.headers().firstValue("X-Len").orElse("-1"));
        }
        Fz.out("sync", sync);
        List<CompletableFuture<String>> fs = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            HttpRequest req = HttpRequest.newBuilder(URI.create("http://127.0.0.1:" + port + "/echo?a=" + i)).GET().build();
            fs.add(client.sendAsync(req, HttpResponse.BodyHandlers.ofString()).thenApply(HttpResponse::body));
        }
        long async = 0;
        for (CompletableFuture<String> f : fs) async = async * 31 + f.get().hashCode();
        Fz.out("async", async);
        server.stop(0);
        ((java.util.concurrent.ExecutorService) server.getExecutor()).shutdown();
        Fz.done();
    }
}
