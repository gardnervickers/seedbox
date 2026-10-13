%% BEAM probes, run by the AArch64 JIT: reduction-counted preemption among processes that never
%% block, message passing (copying, selective receive, monitors) and per-process heaps with
%% their own collections, hibernation and off-heap binaries; hot code loading with processes
%% left in old code (hotCode).
%%
%% Usage: erlfz reductions|heap|hotCode SEED SCALE
-module(beamfz).
-export([main/1, summer/1]).

main([Test, SeedS, ScaleS]) ->
    put(digest, 0),
    try
        Seed = list_to_integer(SeedS),
        Scale = list_to_integer(ScaleS),
        case Test of
            "reductions" -> reductions(Seed, Scale);
            "heap" -> heap(Seed, Scale);
            "hotCode" -> hot_code(Seed, Scale);
            _ -> fail("unknown test " ++ Test)
        end,
        io:format("OK Erl.~s ~8.16.0b~n", [Test, get(digest)]),
        halt(0)
    catch
        C:E:St -> fail(io_lib:format("~p:~p ~p", [C, E, St]))
    end;
main(_) ->
    fail("usage: erlfz reductions|heap|hotCode SEED SCALE").

emit(Key, Value) ->
    Line = lists:flatten(io_lib:format("~s ~p", [Key, Value])),
    io:format("~s~n", [Line]),
    put(digest, erlang:phash2({get(digest), Line}, 1 bsl 32)).

fail(Msg) ->
    io:format("FAIL ~s~n", [Msg]),
    halt(1).

check(What, Got, Want) ->
    Got =:= Want orelse fail(io_lib:format("~s: ~p, want ~p", [What, Got, Want])).

%% ---------------------------------------------------------------------------------------------
%% reductions

%% CPU-bound work: tail calls and arithmetic only, so only reduction counting preempts it.
work(N) -> work(N, 16#9E3779B9, 0).
work(0, X, Acc) -> Acc bxor X;
work(N, X, Acc) ->
    X1 = (X * 1103515245 + 12345) band 16#ffffffff,
    work(N - 1, X1, (Acc + (X1 bsr 7)) band 16#fffffffffff).

fib(N) when N < 2 -> N;
fib(N) -> fib(N - 1) + fib(N - 2).

%% Each process spins on an atomic until the token is its own; with one scheduler every hand-off
%% needs the spinner to run out of reductions.
spin_ring(N, Rounds) ->
    Ref = atomics:new(1, []),
    Self = self(),
    Pids = [spawn(fun() -> spinner(Ref, I, N, Rounds, 0, Self) end) || I <- lists:seq(0, N - 1)],
    [receive {P, Acc} -> Acc end || P <- Pids].

spinner(_Ref, _I, _N, Rounds, Acc, Parent) when Rounds =:= 0 ->
    Parent ! {self(), Acc};
spinner(Ref, I, N, Rounds, Acc, Parent) ->
    Turn = atomics:get(Ref, 1),
    case Turn rem N of
        I ->
            Acc1 = work(500 + I * 10) bxor Acc,
            atomics:add(Ref, 1, 1),
            spinner(Ref, I, N, Rounds - 1, Acc1, Parent);
        _ ->
            spinner(Ref, I, N, Rounds, Acc, Parent)
    end.

spin_want(N, Rounds) ->
    [lists:foldl(fun(_, Acc) -> work(500 + I * 10) bxor Acc end, 0, lists:seq(1, Rounds))
     || I <- lists:seq(0, N - 1)].

ring(M, Laps) ->
    Self = self(),
    First = lists:foldl(fun(_, Next) -> spawn(fun() -> relay(Next) end) end, Self, lists:seq(1, M - 1)),
    First ! {token, 0, M * Laps},
    receive_ring(First, M, Laps).

receive_ring(First, M, Laps) ->
    receive
        {token, V, 0} -> stop_ring(First), V;
        {token, V, K} -> First ! {token, (V * 31 + K) band 16#ffffffff, K - 1}, receive_ring(First, M, Laps)
    end.

relay(Next) ->
    receive
        stop -> Next ! stop;
        {token, V, 0} -> Next ! {token, V, 0}, relay(Next);
        {token, V, K} -> Next ! {token, (V * 31 + K) band 16#ffffffff, K - 1}, relay(Next)
    end.

stop_ring(First) ->
    First ! stop,
    receive stop -> ok end.

ring_want(Total) -> ring_want(0, Total).
ring_want(V, 0) -> V;
ring_want(V, K) -> ring_want((V * 31 + K) band 16#ffffffff, K - 1).

reductions(Seed, Scale) ->
    emit("flavor", erlang:system_info(emu_flavor)),
    rand:seed(exsss, {Seed, 17, 31}),
    Works = [rand:uniform(200000) + 10000 || _ <- lists:seq(1, 300 * Scale)],
    Self = self(),
    Pids = [spawn(fun() -> Self ! {self(), work(W)} end) || W <- Works],
    Results = [receive {P, R} -> R end || P <- Pids],
    check("workers", Results, [work(W) || W <- Works]),
    emit("workers", erlang:phash2(Results)),

    Spin = spin_ring(6, 1000 * Scale),
    check("spin_ring", Spin, spin_want(6, 1000 * Scale)),
    emit("spin_ring", erlang:phash2(Spin)),

    Ring = ring(100, 300 * Scale),
    check("ring", Ring, ring_want(100 * 300 * Scale)),
    emit("ring", Ring),

    %% Results reported as exit reasons through monitors, in completion order, then sorted.
    Mons = [spawn_monitor(fun() -> exit({fib, K, fib(K)}) end) || K <- lists:seq(10, 24)],
    Downs = lists:sort([receive {'DOWN', MRef, process, Pid, Reason} -> Reason end || {Pid, MRef} <- Mons]),
    check("monitors", Downs, [{fib, K, fib(K)} || K <- lists:seq(10, 24)]),
    emit("monitors", erlang:phash2(Downs)),

    %% Selective receive from a mailbox filled in reverse order.
    Count = 2000 * Scale,
    [self() ! {msg, I, I * I} || I <- lists:seq(Count, 1, -1)],
    InOrder = [receive {msg, I, V} -> V end || I <- lists:seq(1, Count)],
    check("selective", InOrder, [I * I || I <- lists:seq(1, Count)]),
    emit("selective", erlang:phash2(InOrder)),

    %% Exceptions raised and caught in JIT code.
    Errs = [catch_kind(I) || I <- lists:seq(1, 5000 * Scale)],
    emit("exceptions", erlang:phash2(lists:sort(maps:to_list(lists:foldl(
        fun(K, M) -> maps:update_with(K, fun(C) -> C + 1 end, 1, M) end, #{}, Errs))))).

catch_kind(I) ->
    try
        case I rem 5 of
            0 -> 1 div (I rem 1);
            1 -> {ok, _} = list_to_tuple([error, I]);
            2 -> element(I, {a});
            3 -> binary_to_integer(<<"x", (integer_to_binary(I))/binary>>);
            4 -> I
        end
    of
        V -> {value, V rem 3}
    catch
        error:badarith -> badarith;
        error:{badmatch, _} -> badmatch;
        error:badarg -> badarg
    end.

%% ---------------------------------------------------------------------------------------------
%% heap

%% Builds and keeps a growing structure, so the process heap grows and is collected many times.
grower(Seed, N, Parent) ->
    rand:seed(exsss, {Seed, 3, 5}),
    Parent ! {self(), summarize(grow(N, [], #{}, <<>>))}.

grow(0, L, M, B) -> {L, M, B};
grow(N, L, M, B) ->
    R = rand:uniform(1 bsl 30),
    L1 = case R rem 2000 of 0 -> []; _ -> [{R, integer_to_list(R)} | L] end,
    M1 = maps:put(R rem 4096, {R, L1 =:= []}, M),
    B1 = case R rem 5000 of 0 -> <<>>; _ -> <<B/binary, (R band 255)>> end,
    _ = case R rem 1000 of 0 -> erlang:garbage_collect(); _ -> ok end,
    grow(N - 1, L1, M1, B1).

summarize({L, M, B}) ->
    erlang:phash2({L, lists:sort(maps:to_list(M)), B}).

grow_want(Seed, N) ->
    rand:seed(exsss, {Seed, 3, 5}),
    summarize(grow(N, [], #{}, <<>>)).

%% A process that sums large messages: every send copies the list into its heap.
summer(Parent) ->
    receive
        {sum, L, Bin} ->
            Parent ! {summed, lists:sum(L), byte_size(Bin), binary:last(Bin)},
            summer(Parent);
        hibernate ->
            erlang:hibernate(?MODULE, summer, [Parent]);
        stop ->
            ok
    end.

heap(Seed, Scale) ->
    Self = self(),
    Seeds = [Seed + I || I <- lists:seq(1, 24)],
    N = 50000 * Scale,
    Opts = fun(I) -> [{min_heap_size, 233 * (1 + I rem 4)}, {fullsweep_after, element(1 + I rem 3, {0, 5, 65535})}] end,
    Pids = [{S, spawn_opt(fun() -> grower(S, N, Self) end, Opts(S))} || S <- Seeds],
    Got = [receive {P, H} -> H end || {_, P} <- Pids],
    check("growers", Got, [grow_want(S, N) || S <- Seeds]),
    emit("growers", erlang:phash2(Got)),

    %% Collections of other processes while they hold large heaps.
    Holders = [spawn(fun() -> hold(lists:seq(1, 50000 + I)) end) || I <- lists:seq(1, 8)],
    [true = erlang:garbage_collect(P) || P <- Holders, _ <- [1, 2]],
    HeldSums = [begin P ! {sum, self()}, receive {held, P, V} -> V end end || P <- Holders],
    check("holders", HeldSums, [lists:sum(lists:seq(1, 50000 + I)) || I <- lists:seq(1, 8)]),
    emit("holders", erlang:phash2(HeldSums)),

    %% Large copied messages and shared off-heap binaries.
    S = spawn(fun() -> summer(Self) end),
    Sums = [begin
                L = lists:seq(1, 10000 + K * 37),
                Bin = binary:copy(<<K:8>>, 100 + K * 1000),
                S ! {sum, L, Bin},
                receive {summed, V, Sz, Last} -> {V, Sz, Last} end
            end || K <- lists:seq(1, 60 * Scale)],
    check("messages", Sums, [{lists:sum(lists:seq(1, 10000 + K * 37)), 100 + K * 1000, K band 255} || K <- lists:seq(1, 60 * Scale)]),
    emit("messages", erlang:phash2(Sums)),
    S ! stop,

    %% Hibernation compacts the heap; the process wakes on the next message.
    H = spawn(fun() -> summer(Self) end),
    Wakes = [begin
                 H ! hibernate,
                 H ! {sum, lists:seq(1, K * 100), <<K>>},
                 receive {summed, V, _, _} -> V end
             end || K <- lists:seq(1, 200)],
    check("hibernate", Wakes, [lists:sum(lists:seq(1, K * 100)) || K <- lists:seq(1, 200)]),
    emit("hibernate", erlang:phash2(Wakes)),

    %% ETS: off-heap terms copied in and out.
    T = ets:new(t, [ordered_set, public]),
    rand:seed(exsss, {Seed, 7, 9}),
    [ets:insert(T, {rand:uniform(100000), I, lists:duplicate(I rem 20, I)}) || I <- lists:seq(1, 50000 * Scale)],
    [ets:delete(T, K) || K <- lists:seq(1, 100000, 3)],
    Sel = ets:select(T, [{{'$1', '$2', '_'}, [{'<', '$1', 5000}], [{{'$1', '$2'}}]}]),
    emit("ets", erlang:phash2({ets:info(T, size), Sel, ets:first(T), ets:last(T)})),
    ets:delete(T).

hold(L) ->
    receive
        {sum, From} -> From ! {held, self(), lists:sum(L)}, hold(L)
    end.

%% ---------------------------------------------------------------------------------------------
%% hotCode

%% Two prebuilt versions of fzhot (fzhot.erl), loaded alternately: each load makes the previous
%% version old code (and frees the version before it, purged first), so the JIT writes new code
%% while processes still run the old. A server process stays in old code behind a message
%% barrier; new external calls must use the new version; soft_purge must refuse while the old
%% code runs and succeed once the server moved on (an external call) or exited; a hard purge kills
%% a process left in old code.

hot_dir() ->
    case os:getenv("FZ_ERL_HOT") of
        false -> "/fuzz/erl/hot";
        D -> D
    end.

hot_load(V) ->
    Path = filename:join([hot_dir(), "v" ++ integer_to_list(V), "fzhot.beam"]),
    {ok, Bin} = file:read_file(Path),
    case code:load_binary(fzhot, Path, Bin) of
        {module, fzhot} -> ok;
        Other -> fail(io_lib:format("load v~p: ~p", [V, Other]))
    end.

hot_k(1) -> 16#9e3779b9;
hot_k(2) -> 16#5bd1e995.

hot_model(_V, X, 0) -> X;
hot_model(V, X, N) -> hot_model(V, ((X bxor hot_k(V)) * 31 + N) band 16#ffffffffff, N - 1).

hot_ping(P, X) ->
    P ! {ping, self(), X},
    receive {pong, P, V, R} -> {V, R} after 60000 -> fail("ping timeout") end.

hot_code(Seed, Scale) ->
    rand:seed(exsss, {Seed, 11, 13}),
    hot_load(1),
    Cycles = 30 * Scale,
    Log = lists:map(fun(K) -> hot_cycle(K) end, lists:seq(1, Cycles)),
    emit("cycles", Cycles),
    emit("log", erlang:phash2(Log)),
    emit("final", fzhot:version()),
    code:purge(fzhot),
    true = code:delete(fzhot),
    code:purge(fzhot),
    emit("unloaded", code:is_loaded(fzhot)).

hot_cycle(K) ->
    Self = self(),
    A = fzhot:version(),
    B = 3 - A,
    X = rand:uniform(1 bsl 40),
    check("external A", fzhot:compute(X, 300), hot_model(A, X, 300)),
    %% A server and a bystander, both in version A.
    P = spawn(fun() -> fzhot:start(Self) end),
    receive {started, P, A} -> ok after 60000 -> fail("start") end,
    Q = spawn(fun() -> fzhot:start(Self) end),
    receive {started, Q, A} -> ok after 60000 -> fail("start q") end,
    QRef = monitor(process, Q),
    check("ping A", hot_ping(P, X), {A, hot_model(A, X, 200)}),
    %% Version A becomes old code: version B is loaded (A's predecessor was purged last cycle).
    check("purged before load", erlang:check_old_code(fzhot), false),
    hot_load(B),
    check("old code present", erlang:check_old_code(fzhot), true),
    check("external B", {fzhot:version(), fzhot:compute(X, 300)}, {B, hot_model(B, X, 300)}),
    %% The server still runs A behind its receive.
    check("ping old", hot_ping(P, X + 1), {A, hot_model(A, X + 1, 200)}),
    check("soft purge refused", code:soft_purge(fzhot), false),
    check("in old code", erlang:check_process_code(P, fzhot), true),
    %% Move the server on: an external call (even cycles) or an exit (odd cycles).
    Mode = K rem 2,
    case Mode of
        0 ->
            P ! {upgrade, Self},
            receive {upgraded, P} -> ok after 60000 -> fail("upgrade") end,
            check("ping new", hot_ping(P, X + 2), {B, hot_model(B, X + 2, 200)}),
            check("moved to new code", erlang:check_process_code(P, fzhot), false);
        1 ->
            P ! {exit, Self},
            receive {bye, P, A} -> ok after 60000 -> fail("bye") end
    end,
    %% The bystander keeps the old code alive: soft purge still refuses, a hard purge kills it.
    check("soft purge with bystander", code:soft_purge(fzhot), false),
    Purged = case K rem 3 of
                 0 ->
                     code:purge(fzhot),
                     receive {'DOWN', QRef, process, Q, Reason} -> Reason after 60000 -> fail("purge kill") end;
                 _ ->
                     Q ! {upgrade, Self},
                     receive {upgraded, Q} -> ok after 60000 -> fail("upgrade q") end,
                     check("soft purge", code:soft_purge(fzhot), true),
                     Q ! {exit, Self},
                     receive {bye, Q, B} -> ok after 60000 -> fail("bye q") end,
                     receive {'DOWN', QRef, process, Q, normal} -> soft after 60000 -> fail("q down") end
             end,
    check("no old code", erlang:check_old_code(fzhot), false),
    case Mode of
        0 -> P ! {exit, Self}, receive {bye, P, B} -> ok after 60000 -> fail("bye new") end;
        1 -> ok
    end,
    {K, A, B, Mode, Purged}.
