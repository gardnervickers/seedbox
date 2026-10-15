%% The module beamfz's hotCode test loads in two versions, compiled from this file twice
%% (erlc for version 1, erlc -DV2 for version 2) into directories outside the code path.
%% Local calls stay in the version a process runs; the external call fzhot:server/1 moves it to
%% the current version.
-module(fzhot).
-export([version/0, compute/2, start/1, server/1]).

-ifdef(V2).
-define(VER, 2).
-define(K, 16#5bd1e995).
-else.
-define(VER, 1).
-define(K, 16#9e3779b9).
-endif.

version() -> ?VER.

%% Hot arithmetic, different in each version.
compute(X, 0) -> X;
compute(X, N) -> compute(((X bxor ?K) * 31 + N) band 16#ffffffffff, N - 1).

start(Parent) ->
    Parent ! {started, self(), version()},
    server(Parent).

server(Parent) ->
    receive
        {ping, From, X} ->
            From ! {pong, self(), version(), compute(X, 200)},
            server(Parent);
        {upgrade, From} ->
            From ! {upgraded, self()},
            fzhot:server(Parent);
        {exit, From} ->
            From ! {bye, self(), version()}
    end.
