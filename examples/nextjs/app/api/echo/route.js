export async function POST(request) {
  return Response.json({ body: await request.text() });
}
