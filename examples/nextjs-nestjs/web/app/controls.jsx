'use client';
import { useState, useActionState } from 'react';
import { submit } from './actions';

export default function Controls() {
  const [count, setCount] = useState(0);
  const [identity, setIdentity] = useState(null);
  const [action, formAction, pending] = useActionState(submit, null);
  return <>
    <button onClick={() => setCount(count + 1)}>Count {count}</button>
    <button onClick={async () => setIdentity(await fetch('/bff/identity').then(r => r.json()))}>Call BFF</button>
    <pre id="bff-result">{JSON.stringify(identity)}</pre>
    <form action={formAction}>
      <input name="value" aria-label="Action value" defaultValue="hello action" />
      <button disabled={pending}>Submit action</button>
    </form>
    <pre id="action-result">{JSON.stringify(action)}</pre>
  </>;
}
