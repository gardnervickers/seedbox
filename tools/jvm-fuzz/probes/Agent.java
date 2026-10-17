import java.lang.instrument.Instrumentation;

/** Java agent (agent.jar, built by the image) that hands its Instrumentation to Redefine. */
public final class Agent {
    static volatile Instrumentation inst;

    public static void premain(String args, Instrumentation i) { inst = i; }
}
