import Link from 'next/link';
import Counter from './counter';

export const dynamic = 'force-dynamic';

export default function Page() {
  return (
    <main>
      <h1>Next.js over HTTP/3</h1>
      <p>Click the counter to check hydration, then navigate without a full reload.</p>
      <Counter />
      <p><Link href="/about" prefetch={false}>About</Link></p>
    </main>
  );
}
