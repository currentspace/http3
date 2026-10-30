import { bffIdentity } from '../../../lib/bff';
export async function GET(request) {
  return Response.json(await bffIdentity(request.headers.get('cookie')));
}
