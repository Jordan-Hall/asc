import { startDevHost } from "./server";

const host = await startDevHost({ port: Number(process.env.PORT ?? 4173) });
process.stdout.write(`asc-ui dev host at ${host.url}\n`);
