import Link from 'next/link';
import { cookies, headers } from 'next/headers';
import { bffIdentity } from '../lib/bff';
import Controls from './controls';

export const dynamic = 'force-dynamic';
export default async function Page() {
  const session = (await cookies()).get('demo-session')?.value ?? 'none';
  const identity = await bffIdentity((await headers()).get('cookie'));
  return <main>
    <h1>Next.js and NestJS with Express over HTTP/3</h1>
    <p id="server-pid">{process.pid}</p>
    <p id="server-session">{session}</p>
    <p id="server-bff-pid">{identity.pid}</p>
    <p id="server-bff-version">{identity.httpVersion}</p>
    <p id="server-bff-session">{identity.session ?? 'none'}</p>
    <Controls />
    <Link href="/about" prefetch={false}>About</Link>
  </main>;
}
