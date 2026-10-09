import java.net.InetAddress;

class RuntimeHostname {
    public static void main(String[] args) throws Exception {
        String hostname = InetAddress.getLocalHost().getHostName();
        if (!hostname.equals("jvm")) throw new AssertionError(hostname);
        System.out.println("JVM local hostname passed: " + hostname);
    }
}
