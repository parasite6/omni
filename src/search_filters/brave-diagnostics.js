// Opt-in metadata only: never log query strings, request bodies, or answer text.
(() => {
  const log = data => console.info('omni-search-diagnostic:' + JSON.stringify(data));
  new PerformanceObserver(list => {
    for (const entry of list.getEntries()) {
      const url=new URL(entry.name,location.href);
      if (url.origin===location.origin && url.pathname.startsWith('/api/tap/')) {
        log({event:'transport',path:url.pathname,protocol:entry.nextHopProtocol,status:entry.responseStatus});
      }
    }
  }).observe({type:'resource',buffered:true});
  const original = window.fetch;
  window.fetch = async function(input, init) {
    const url = new URL(typeof input === 'string' ? input : input.url, location.href);
    if (url.origin !== location.origin || !url.pathname.startsWith('/api/tap/')) {
      return original.call(this, input, init);
    }
    const start = performance.now();
    const path = url.pathname;
    log({event:'request', path, parameterNames:[...url.searchParams.keys()]});
    if (path.endsWith('/new')) {
      const formats={};
      for (const name of ['symmetric_key','nonce','sig']) {
        const value=url.searchParams.get(name)||'';
        formats[name]={length:value.length,plus:value.includes('+'),slash:value.includes('/'),
          underscore:value.includes('_'),hyphen:value.includes('-'),space:/\s/.test(value)};
      }
      log({event:'request-format',path,urlLength:url.href.length,formats});
    }
    try {
      const response = await original.call(this, input, init);
      log({event:'headers', path, status:response.status, ms:Math.round(performance.now()-start)});
      if (!response.ok) response.clone().text().then(text => {
        if (text.trim().startsWith('<')) {
          const page=new DOMParser().parseFromString(text,'text/html');
          page.querySelectorAll('script,style').forEach(e=>e.remove());
          text=(page.querySelector('main') || page.body).textContent.replace(/\s+/g,' ').trim();
        }
        const reason=/Bad request syntax or unsupported method/.test(text) ? 'bad-request' :
          /timed out|timeout/i.test(text) ? 'timeout' : /captcha/i.test(text) ? 'captcha' : 'other';
        log({event:'http-error',path,status:response.status,reason});
      });
      if (path.includes('stream') && response.body) {
        (async () => {
          let bytes = 0, first = true, tail = '';
          const reader = response.clone().body.getReader();
          const decoder = new TextDecoder();
          try {
            for (;;) {
              const {done, value} = await reader.read();
              if (done) break;
              bytes += value.byteLength;
              if (first) { first=false; log({event:'first-chunk', path, ms:Math.round(performance.now()-start)}); }
              tail += decoder.decode(value, {stream:true});
              let split;
              while ((split=tail.indexOf('\n')) >= 0) {
                const line=tail.slice(0,split); tail=tail.slice(split+1);
                try {
                  const item=JSON.parse(line);
                  if (item.type === 'error') log({event:'stream-error-record',path,keys:Object.keys(item),timeout:/timeout|timed out/i.test(line)});
                } catch (_) {}
              }
              if (tail.length>65536) tail='';
            }
            log({event:'end', path, bytes, ms:Math.round(performance.now()-start)});
          } catch (error) {
            log({event:'stream-failed',path,name:error.name,bytes,ms:Math.round(performance.now()-start)});
          }
        })();
      }
      return response;
    } catch (error) {
      log({event:'fetch-failed',path,name:error.name,ms:Math.round(performance.now()-start)});
      throw error;
    }
  };
  for (const ms of [2000,12000,32000,42000]) setTimeout(() => {
    const tracks=[...document.querySelectorAll('.vscroll-track')];
    log({event:'page-state',ms,pageKind:document.title==='Captcha - Brave Search'?'verification':document.querySelector('.ask-center')?'ask':'other',filter:!!document.getElementById('omni-page-filter'),
      customTracks:tracks.length,visibleTracks:tracks.filter(e=>getComputedStyle(e).display!=='none').length,
      messages:[...document.querySelectorAll('.message')].map(e=>({class:e.className,chars:e.textContent.length}))});
  }, ms);
})();
