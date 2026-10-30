import Link from 'next/link';
export const dynamic = 'force-dynamic';
export default function About() {
  return <main><h1>Client navigation works</h1><p id="server-pid">{process.pid}</p><Link href="/">Home</Link></main>;
}
