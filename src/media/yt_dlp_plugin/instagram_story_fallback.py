# Temporary fallback for yt-dlp/yt-dlp#17755: the reels API the built-in story extractor asks
# answers without reels even for a signed-in session, so every story ends with "You need to log
# in to access this content". The story page itself carries the same reels in its embedded data.
# The built-in extractor always runs first; this file only acts when it gives up for that reason,
# and is removed once a yt-dlp release fixes the issue.
import json
import re

from yt_dlp.extractor.instagram import InstagramStoryIE, _pk_to_id
from yt_dlp.utils import ExtractorError, filter_dict, traverse_obj

_LOGIN_MESSAGES = (
    'You need to log in to access this content',
    'This content is unreachable',
)
_EMBEDDED_DATA = re.compile(r'<script\b[^>]*\bdata-sjs\b[^>]*>(.*?)</script>', re.DOTALL)
_REELS_KEY = 'xdt_api__v1__feed__reels_media'


def embedded_reels(webpage):
    """Every reel in the page's embedded data, from both the list and the connection form."""
    pending = []
    for blob in _EMBEDDED_DATA.findall(webpage):
        try:
            pending.append(json.loads(blob))
        except ValueError:
            continue
    while pending:
        value = pending.pop()
        if isinstance(value, list):
            pending.extend(value)
            continue
        if not isinstance(value, dict):
            continue
        for key, child in value.items():
            if key.startswith(_REELS_KEY) and isinstance(child, dict):
                yield from traverse_obj(child, ('reels_media', lambda _, v: v['items']))
                yield from traverse_obj(child, ('edges', lambda _, v: v['node']['items'], 'node'))
            else:
                pending.append(child)


class _InstagramStoryFallbackIE(InstagramStoryIE, plugin_name='ssdownload'):
    def _real_extract(self, url):
        try:
            return super()._real_extract(url)
        except ExtractorError as error:
            if not error.orig_msg.startswith(_LOGIN_MESSAGES):
                raise
            original = error
        try:
            result = self._extract_embedded(url)
        except Exception as error:
            self.write_debug(f'Embedded story data could not be used: {error}')
            result = None
        if not result:
            raise original
        return result

    def _extract_embedded(self, url):
        username, story_id = self._match_valid_url(url).group('user', 'id')
        display_id = story_id or username
        webpage = self._download_webpage(
            url, display_id, 'Downloading story page for embedded reels',
            impersonate=self._can_impersonate and self._is_web_app)
        return self._story_result(username, story_id, embedded_reels(webpage))

    @staticmethod
    def _select_reel(username, story_id, reels):
        reels = list(reels)
        if username == 'highlights':
            return next((r for r in reels if str(r.get('id')) in (f'highlight:{story_id}', story_id)), None)

        def holds_story(reel):
            return any(
                str(item.get('pk') or str(item.get('id') or '').split('_')[0]) == story_id
                for item in traverse_obj(reel, ('items', ..., {dict})))

        def owned_by_user(reel):
            return str(traverse_obj(reel, ('user', 'username')) or '').lower() == username.lower()

        return (story_id and next(filter(holds_story, reels), None)) or next(filter(owned_by_user, reels), None)

    def _story_result(self, username, story_id, reels):
        reel = self._select_reel(username, story_id, reels)
        if not reel:
            return None

        user_info = traverse_obj(reel, ('user', {dict})) or {}
        user_id = traverse_obj(user_info, 'pk', 'id', expected_type=str)
        title = (username == 'highlights' and reel.get('title')) or f'Story by {username}'
        entries = []
        for item in traverse_obj(reel, ('items', ..., {dict})):
            if not isinstance(item.get('user'), dict):
                item['user'] = {}
            item['user'].update(user_info)
            data = self._extract_product(item)
            if data.get('formats'):
                entries.append({
                    'uploader': user_info.get('full_name'),
                    'uploader_id': user_id,
                    **filter_dict(data),
                })
        if not entries:
            return None
        if username != 'highlights' and story_id and not self._yes_playlist(username, story_id):
            return traverse_obj(entries, (lambda _, v: v['id'] == _pk_to_id(story_id), any))
        return self.playlist_result(entries, playlist_id=story_id, playlist_title=title)
