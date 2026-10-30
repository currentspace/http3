import { cookies } from 'next/headers';
export async function GET() {
  const jar = await cookies();
  return Response.json({ session: jar.get('demo-session')?.value ?? null,
    preference: jar.get('demo-preference')?.value ?? null, pid: process.pid });
}
