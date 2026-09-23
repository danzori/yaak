import { useMutation, useQuery } from "@tanstack/react-query";
import { platform } from "@yaakapp-internal/platform";
import type { GrpcConnection, GrpcRequest } from "@yaakapp-internal/models";
import { flushAllModelWrites } from "@yaakapp-internal/models";
import { jotaiStore } from "../lib/jotai";
import { minPromiseMillis } from "../lib/minPromiseMillis";
import { rpc } from "../lib/rpc";
import { activeEnvironmentIdAtom, useActiveEnvironment } from "./useActiveEnvironment";
import { useDebouncedValue } from "@yaakapp-internal/ui";
import { atom } from "jotai";

export interface ReflectResponseService {
  name: string;
  methods: { name: string; schema: string; serverStreaming: boolean; clientStreaming: boolean }[];
}

export const grpcReflectLogsAtom = atom<Record<string, string[]>>({});

export function useGrpc(
  req: GrpcRequest | null,
  conn: GrpcConnection | null,
  protoFiles: string[],
) {
  const requestId = req?.id ?? "n/a";
  const environment = useActiveEnvironment();

  const go = useMutation<void, string>({
    mutationKey: ["grpc_go", conn?.id],
    mutationFn: async () => {
      await flushAllModelWrites(); // The backend reads the request from the DB
      return rpc<void>("cmd_grpc_go", {
        requestId,
        environmentId: environment?.id,
        protoFiles,
      });
    },
  });

  const send = useMutation({
    mutationKey: ["grpc_send", conn?.id],
    mutationFn: ({ message }: { message: string }) =>
      platform.emit(`grpc_client_msg_${conn?.id ?? "none"}`, { Message: message }),
  });

  const cancel = useMutation({
    mutationKey: ["grpc_cancel", conn?.id ?? "n/a"],
    mutationFn: () => platform.emit(`grpc_client_msg_${conn?.id ?? "none"}`, "Cancel"),
  });

  const commit = useMutation({
    mutationKey: ["grpc_commit", conn?.id ?? "n/a"],
    mutationFn: () => platform.emit(`grpc_client_msg_${conn?.id ?? "none"}`, "Commit"),
  });

  const debouncedUrl = useDebouncedValue<string>(req?.url ?? "", 1000);

  const reflect = useQuery<ReflectResponseService[], string>({
    enabled: req != null,
    queryKey: ["grpc_reflect", req?.id ?? "n/a", debouncedUrl, protoFiles],
    staleTime: Infinity,
    refetchOnMount: false,
    refetchOnWindowFocus: false,
    refetchOnReconnect: false,
    queryFn: () => {
      const environmentId = jotaiStore.get(activeEnvironmentIdAtom);
      const setLogs = (update: (logs: string[]) => string[]) =>
        jotaiStore.set(grpcReflectLogsAtom, (all) => ({
          ...all,
          [requestId]: update(all[requestId] ?? []),
        }));
      setLogs(() => []);
      const reflect = platform
        .rpcStream<ReflectResponseService[], string>(
          "cmd_grpc_reflect",
          { requestId, protoFiles, environmentId },
          (line) => setLogs((logs) => [...logs, line]),
        )
        .then(({ result, unlisten }) => {
          unlisten();
          return result;
        })
        .catch((err) => {
          setLogs((logs) => [...logs, `Error: ${err}`]);
          throw err;
        });
      return minPromiseMillis<ReflectResponseService[]>(reflect, 300);
    },
  });

  return {
    go,
    reflect,
    cancel,
    commit,
    isStreaming: conn != null && conn.state !== "closed",
    send,
  };
}
