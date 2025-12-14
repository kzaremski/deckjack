# JackLink - Portable Computing Gateway
# Copyright (c) 2025 Konstantin Zaremski
# Licensed under the MIT License

import re
from html.parser import HTMLParser
from urllib.parse import urljoin, urlparse, quote, unquote
from typing import Optional, Tuple
import httpx


# Attributes that contain URLs
URL_ATTRIBUTES = {
    'a': ['href'],
    'img': ['src', 'srcset'],
    'script': ['src'],
    'link': ['href'],
    'form': ['action'],
    'iframe': ['src'],
    'video': ['src', 'poster'],
    'audio': ['src'],
    'source': ['src', 'srcset'],
    'object': ['data'],
    'embed': ['src'],
    'area': ['href'],
    'input': ['src'],
    'track': ['src'],
}

# Tags where we shouldn't rewrite content
SKIP_CONTENT_TAGS = {'script', 'style', 'noscript'}


def make_proxy_url(url: str, base_url: str) -> str:
    """Convert a URL to go through our proxy."""
    if not url or url.startswith('data:') or url.startswith('javascript:') or url.startswith('#'):
        return url

    # Handle protocol-relative URLs
    if url.startswith('//'):
        parsed_base = urlparse(base_url)
        url = f"{parsed_base.scheme}:{url}"

    # Make absolute URL
    absolute_url = urljoin(base_url, url)

    # Encode for proxy
    return f"/api/proxy?url={quote(absolute_url, safe='')}"


def rewrite_srcset(srcset: str, base_url: str) -> str:
    """Rewrite srcset attribute which has multiple URLs."""
    parts = []
    for part in srcset.split(','):
        part = part.strip()
        if not part:
            continue
        # srcset format: "url descriptor" or just "url"
        tokens = part.split()
        if tokens:
            tokens[0] = make_proxy_url(tokens[0], base_url)
            parts.append(' '.join(tokens))
    return ', '.join(parts)


class URLRewriter(HTMLParser):
    """HTML parser that rewrites URLs to go through proxy."""

    def __init__(self, base_url: str):
        super().__init__()
        self.base_url = base_url
        self.output = []
        self.in_skip_tag = None

    def handle_starttag(self, tag: str, attrs: list):
        tag_lower = tag.lower()

        if tag_lower in SKIP_CONTENT_TAGS:
            self.in_skip_tag = tag_lower

        # Rewrite URL attributes
        new_attrs = []
        url_attrs = URL_ATTRIBUTES.get(tag_lower, [])

        for name, value in attrs:
            if value is None:
                new_attrs.append((name, None))
                continue

            name_lower = name.lower()
            if name_lower in url_attrs:
                if name_lower == 'srcset':
                    value = rewrite_srcset(value, self.base_url)
                else:
                    value = make_proxy_url(value, self.base_url)
            new_attrs.append((name, value))

        # Reconstruct tag
        attr_str = ''
        for name, value in new_attrs:
            if value is None:
                attr_str += f' {name}'
            else:
                # Escape quotes in value
                escaped = value.replace('"', '&quot;')
                attr_str += f' {name}="{escaped}"'

        self.output.append(f'<{tag}{attr_str}>')

    def handle_endtag(self, tag: str):
        if tag.lower() == self.in_skip_tag:
            self.in_skip_tag = None
        self.output.append(f'</{tag}>')

    def handle_startendtag(self, tag: str, attrs: list):
        tag_lower = tag.lower()

        # Rewrite URL attributes
        new_attrs = []
        url_attrs = URL_ATTRIBUTES.get(tag_lower, [])

        for name, value in attrs:
            if value is None:
                new_attrs.append((name, None))
                continue

            name_lower = name.lower()
            if name_lower in url_attrs:
                if name_lower == 'srcset':
                    value = rewrite_srcset(value, self.base_url)
                else:
                    value = make_proxy_url(value, self.base_url)
            new_attrs.append((name, value))

        # Reconstruct tag
        attr_str = ''
        for name, value in new_attrs:
            if value is None:
                attr_str += f' {name}'
            else:
                escaped = value.replace('"', '&quot;')
                attr_str += f' {name}="{escaped}"'

        self.output.append(f'<{tag}{attr_str} />')

    def handle_data(self, data: str):
        self.output.append(data)

    def handle_comment(self, data: str):
        self.output.append(f'<!--{data}-->')

    def handle_decl(self, decl: str):
        self.output.append(f'<!{decl}>')

    def handle_pi(self, data: str):
        self.output.append(f'<?{data}>')

    def handle_entityref(self, name: str):
        self.output.append(f'&{name};')

    def handle_charref(self, name: str):
        self.output.append(f'&#{name};')

    def get_output(self) -> str:
        return ''.join(self.output)


def rewrite_css(content: str, base_url: str) -> str:
    """Rewrite URLs in CSS content."""
    def replace_url(match):
        url = match.group(1) or match.group(2) or match.group(3)
        if url and not url.startswith('data:'):
            proxy_url = make_proxy_url(url, base_url)
            return f'url("{proxy_url}")'
        return match.group(0)

    # Match url("..."), url('...'), and url(...)
    pattern = r'url\(\s*"([^"]+)"\s*\)|url\(\s*\'([^\']+)\'\s*\)|url\(\s*([^)]+?)\s*\)'
    content = re.sub(pattern, replace_url, content)

    # Handle @import "..." and @import '...'
    def replace_import(match):
        url = match.group(1) or match.group(2)
        if url:
            proxy_url = make_proxy_url(url, base_url)
            return f'@import "{proxy_url}"'
        return match.group(0)

    import_pattern = r'@import\s+"([^"]+)"|@import\s+\'([^\']+)\''
    content = re.sub(import_pattern, replace_import, content)

    return content


def rewrite_html(content: str, base_url: str) -> str:
    """Rewrite HTML content with proxied URLs."""
    try:
        parser = URLRewriter(base_url)
        parser.feed(content)
        result = parser.get_output()

        # Also rewrite inline styles
        def replace_style(match):
            style_content = match.group(1)
            rewritten = rewrite_css(style_content, base_url)
            return f'<style>{rewritten}</style>'

        result = re.sub(r'<style[^>]*>(.*?)</style>', replace_style, result, flags=re.DOTALL | re.IGNORECASE)

        # Inject helper script for intercepting dynamic navigation
        inject_script = '''
<script>
(function() {
    // Extract real URL from proxy URL
    function extractRealUrl(url) {
        if (url && url.includes('/api/proxy?url=')) {
            var match = url.match(/[?&]url=([^&]+)/);
            if (match) {
                return decodeURIComponent(match[1]);
            }
        }
        return url;
    }

    // Intercept link clicks
    document.addEventListener('click', function(e) {
        var target = e.target.closest('a[href]');
        if (target && target.href) {
            var href = target.getAttribute('href');
            if (href && !href.startsWith('#') && !href.startsWith('javascript:')) {
                e.preventDefault();
                var realUrl = extractRealUrl(target.href);
                window.parent.postMessage({type: 'navigate', url: realUrl}, '*');
            }
        }
    }, true);

    // Intercept form submissions
    document.addEventListener('submit', function(e) {
        var form = e.target;
        if (form.tagName === 'FORM') {
            e.preventDefault();
            var formData = new FormData(form);
            var action = form.getAttribute('action') || '';
            var method = (form.method || 'GET').toUpperCase();

            // Get the real action URL (not the proxied one)
            var realAction = extractRealUrl(form.action || window.location.href);

            window.parent.postMessage({
                type: 'form',
                url: realAction,
                method: method,
                data: Object.fromEntries(formData)
            }, '*');
        }
    }, true);
})();
</script>
'''
        # Insert before </body> or at end
        if '</body>' in result.lower():
            result = re.sub(r'(</body>)', inject_script + r'\1', result, flags=re.IGNORECASE)
        else:
            result += inject_script

        return result
    except Exception as e:
        # If parsing fails, return original with error comment
        return f'<!-- JackLink proxy parse error: {e} -->\n{content}'


async def fetch_url(
    url: str,
    method: str = 'GET',
    headers: Optional[dict] = None,
    body: Optional[bytes] = None,
    cookies: Optional[dict] = None,
    timeout: float = 30.0,
) -> Tuple[int, dict, bytes, str]:
    """
    Fetch a URL and return (status_code, headers, content, final_url).
    """
    request_headers = {
        'User-Agent': 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36',
        'Accept': 'text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8',
        'Accept-Language': 'en-US,en;q=0.5',
        'Accept-Encoding': 'gzip, deflate',
    }

    if headers:
        request_headers.update(headers)

    # Remove headers that shouldn't be forwarded
    for h in ['host', 'origin', 'referer', 'cookie']:
        request_headers.pop(h, None)
        request_headers.pop(h.title(), None)

    async with httpx.AsyncClient(
        follow_redirects=True,
        timeout=timeout,
        verify=False,  # Allow self-signed certs for internal sites
    ) as client:
        if method.upper() == 'GET':
            response = await client.get(url, headers=request_headers, cookies=cookies)
        elif method.upper() == 'POST':
            response = await client.post(url, headers=request_headers, content=body, cookies=cookies)
        else:
            response = await client.request(method, url, headers=request_headers, content=body, cookies=cookies)

        # Get response headers (convert to dict)
        resp_headers = dict(response.headers)

        return response.status_code, resp_headers, response.content, str(response.url)


def get_content_type(headers: dict) -> str:
    """Extract content type from headers."""
    ct = headers.get('content-type', headers.get('Content-Type', ''))
    return ct.split(';')[0].strip().lower()


def should_rewrite(content_type: str) -> bool:
    """Check if content should be rewritten."""
    return content_type in ('text/html', 'text/css', 'application/xhtml+xml')
