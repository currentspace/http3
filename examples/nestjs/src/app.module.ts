import {
  Body, Controller, ForbiddenException, Get, Injectable, Module, Param,
  ParseIntPipe, Post, Req, Sse, StreamableFile,
} from '@nestjs/common';
import type { MessageEvent, RawBodyRequest } from '@nestjs/common';
import type { FastifyRequest } from 'fastify';
import { createHash } from 'node:crypto';
import { Readable } from 'node:stream';
import { interval, map, take } from 'rxjs';

@Injectable()
class GreetingService {
  greet(name: string) { return `Hello, ${name}`; }
}

@Controller()
class AppController {
  constructor(private readonly greetings: GreetingService) {}

  @Get()
  index(@Req() request: FastifyRequest) {
    return { message: this.greetings.greet('NestJS'), httpVersion: request.raw.httpVersion };
  }

  @Get('items/:id')
  item(@Param('id', ParseIntPipe) id: number) { return { id }; }

  @Post('echo')
  echo(@Body() body: unknown, @Req() request: RawBodyRequest<FastifyRequest>) {
    return {
      body,
      httpVersion: request.raw.httpVersion,
      rawBodySha256: createHash('sha256').update(request.rawBody!).digest('hex'),
    };
  }

  @Get('forbidden')
  forbidden(): never { throw new ForbiddenException('Example denial'); }

  @Get('download')
  download() {
    return new StreamableFile(Readable.from((function* () {
      for (let i = 0; i < 16; i++) yield Buffer.alloc(16 * 1024, i);
    })()), { type: 'application/octet-stream', disposition: 'attachment; filename="sample.bin"' });
  }

  @Sse('events')
  events() {
    return interval(100).pipe(take(3), map((id): MessageEvent => ({ data: { id } })));
  }
}

@Module({ controllers: [AppController], providers: [GreetingService] })
export class AppModule {}
