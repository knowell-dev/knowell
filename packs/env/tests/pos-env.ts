export const config = {
  port: Number(process.env.PORT ?? 3000),
  dsn: process.env["DOCK_DATABASE_URL"],
  region: import.meta.env.VITE_REGION,
};

export function flag(name: string) {
  return process.env[name];
}
