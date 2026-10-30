'use server';
export async function submit(_previous, form) {
  return { value: String(form.get('value') ?? ''), pid: process.pid };
}
