import Link from 'next/link';
import Counter from './counter';
export const dynamic = 'force-dynamic';
export default function Page() {
  return <main><h1>Next.js over HTTP/3</h1><Counter /><Link href="/about" prefetch={false}>About</Link></main>;
}
