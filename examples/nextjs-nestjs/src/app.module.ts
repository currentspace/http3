import {
  Body, Controller, Get, Injectable, Module, Param, ParseIntPipe, Post,
  Req, Res, Sse, StreamableFile, UseGuards,
} from '@nestjs/common';
import type { CanActivate, ExecutionContext, MessageEvent, RawBodyRequest } from '@nestjs/common';
import type { Request, Response } from 'express';
import { createHash, randomUUID } from 'node:crypto';
import { Readable } from 'node:stream';
import { interval, map, take } from 'rxjs';

@Injectable()
class IdentityService {
  identity(req: Request) {
    return { framework: 'nestjs-express', pid: process.pid, httpVersion: req.httpVersion,
      secure: req.secure, session: (req.cookies as Record<string, string>)['demo-session'] ?? null,
      preference: (req.cookies as Record<string, string>)['demo-preference'] ?? null,
      cookieHeader: req.headers.cookie ?? null,
      rawCookieFields: req.rawHeaders.filter((name, index) => index % 2 === 0 && name.toLowerCase() === 'cookie').length };
  }
}

// Demonstrates Nest guard execution, not an authentication scheme.
@Injectable()
class DemoGuard implements CanActivate {
  canActivate(context: ExecutionContext) {
    return context.switchToHttp().getRequest<Request>().headers['x-demo-access'] === 'allowed';
  }
}

@Controller()
class BffController {
  constructor(private readonly identities: IdentityService) {}

  @Get('identity')
  identity(@Req() req: Request) { return this.identities.identity(req); }

  @Get('items/:id')
  item(@Param('id', ParseIntPipe) id: number) { return { id }; }

  @Get('guarded')
  @UseGuards(DemoGuard)
  guarded() { return { allowed: true }; }

  @Post('session')
  session(@Res({ passthrough: true }) res: Response) {
    const session = randomUUID();
    res.cookie('demo-session', session, { httpOnly: true, secure: true, sameSite: 'lax', path: '/' });
    res.cookie('demo-preference', 'compact', { secure: true, sameSite: 'lax', path: '/' });
    return { session };
  }

  @Post('echo')
  echo(@Body() body: unknown, @Req() req: RawBodyRequest<Request>) {
    return { body, pid: process.pid, httpVersion: req.httpVersion,
      rawBodySha256: createHash('sha256').update(req.rawBody!).digest('hex') };
  }

  @Post('upload')
  async upload(@Req() req: Request) {
    const hash = createHash('sha256');
    let bytes = 0;
    // application/octet-stream bypasses the JSON/urlencoded parsers. Consume
    // incrementally instead of retaining the entire upload in memory.
    for await (const data of req) {
      const chunk = data as Buffer;
      bytes += chunk.length;
      hash.update(chunk);
      if (process.env.HTTP3_TEST_PROGRESS === '1' && bytes === chunk.length) {
        console.log('UPLOAD_PREFIX ' + JSON.stringify({ id: req.headers['x-upload-id'], bytes }));
      }
    }
    return { bytes, sha256: hash.digest('hex'), pid: process.pid, httpVersion: req.httpVersion };
  }

  @Get('download')
  download() {
    return new StreamableFile(Readable.from((function* () {
      for (let i = 0; i < 16; i++) yield Buffer.alloc(16384, i);
    })()), { type: 'application/octet-stream', disposition: 'attachment; filename="sample.bin"' });
  }

  @Sse('events')
  events() {
    return interval(100).pipe(take(3), map((id): MessageEvent => ({ data: { id, pid: process.pid } })));
  }
}

@Module({ controllers: [BffController], providers: [IdentityService, DemoGuard] })
export class AppModule {}
