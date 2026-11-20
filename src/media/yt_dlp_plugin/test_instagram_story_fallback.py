"""Tests for instagram_story_fallback.py against an installed yt-dlp of the version the app ships.

    uv run --no-project --with yt-dlp==2026.10.26 python src/media/yt_dlp_plugin/test_instagram_story_fallback.py
"""
import importlib.util
import json
import pathlib
import unittest
from unittest import mock

from yt_dlp import YoutubeDL
from yt_dlp.extractor.instagram import InstagramStoryIE, _pk_to_id
from yt_dlp.utils import ExtractorError

_spec = importlib.util.spec_from_file_location(
    'instagram_story_fallback', pathlib.Path(__file__).with_name('instagram_story_fallback.py'))
plugin = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(plugin)
BuiltIn = InstagramStoryIE

VIDEO_PK = '3570766765028588805'
IMAGE_PK = '3570766765028588806'
OTHER_PK = '3570766765028588807'
STORY_URL = f'https://www.instagram.com/stories/someone/{VIDEO_PK}/'


def video_item(pk, url):
    return {
        'pk': pk,
        'taken_at': 1700000000,
        'video_duration': 5.5,
        'video_versions': [
            {'id': 'v720', 'url': url, 'width': 720, 'height': 1280},
            {'id': 'v480', 'url': url + '?low', 'width': 480, 'height': 854},
        ],
        'image_versions2': {'candidates': [{'url': 'https://cdn.example/poster.jpg', 'width': 720, 'height': 1280}]},
    }


def image_item(pk):
    return {
        'pk': pk,
        'taken_at': 1700000001,
        'image_versions2': {'candidates': [{'url': 'https://cdn.example/photo.jpg', 'width': 1080, 'height': 1920}]},
    }


def reel(reel_id, username, items, **extra):
    return {
        'id': reel_id,
        'user': {'pk': '1111', 'username': username, 'full_name': 'Some One'},
        'items': items,
        **extra,
    }


def page(data):
    """A story page whose embedded data carries `data` the way the preloaded relay query does."""
    blob = {'require': [['ScheduledServerJS', 'handle', None, [{'__bbox': {'require': [[
        'RelayPrefetchedStreamCache', 'next', [],
        ['adp_PolarisStoriesV3ReelPageStandaloneQueryRelayPreloader', {'__bbox': {
            'complete': True, 'result': {'data': data}}}],
    ]]}}]]]}
    return (
        '<html><head><script type="application/json">{"unrelated": true}</script></head><body>'
        f'<script type="application/json" data-content-len="1" data-sjs>{json.dumps(blob)}</script>'
        '</body></html>')


def user_stories_page():
    return page({'xdt_api__v1__feed__reels_media': {'reels_media': [
        reel('2222', 'somebody_else', [video_item(OTHER_PK, 'https://cdn.example/other.mp4')]),
        reel('1111', 'Someone', [video_item(VIDEO_PK, 'https://cdn.example/story.mp4'), image_item(IMAGE_PK)]),
    ]}})


def extractor(**params):
    ie = plugin._InstagramStoryFallbackIE(YoutubeDL({'quiet': True, 'no_warnings': True, **params}))
    ie._can_impersonate = False
    return ie


def login_required(self, url):
    self.raise_login_required('You need to log in to access this content')


class EmbeddedReels(unittest.TestCase):
    def test_a_user_story_becomes_a_playlist_of_its_video_items(self):
        result = extractor()._story_result('someone', None, plugin.embedded_reels(user_stories_page()))

        self.assertEqual(result['_type'], 'playlist')
        self.assertEqual(result['title'], 'Story by someone')
        self.assertEqual([entry['id'] for entry in result['entries']], [_pk_to_id(VIDEO_PK)])
        entry = result['entries'][0]
        self.assertEqual(
            [(f['format_id'], f['url'], f['width'], f['height']) for f in entry['formats']],
            [('v720', 'https://cdn.example/story.mp4', 720, 1280),
             ('v480', 'https://cdn.example/story.mp4?low', 480, 854)])
        self.assertEqual(entry['duration'], 5.5)
        self.assertEqual(entry['timestamp'], 1700000000)
        self.assertEqual(entry['uploader'], 'Some One')
        self.assertEqual(entry['uploader_id'], '1111')
        self.assertEqual(entry['channel'], 'Someone')
        self.assertEqual(entry['thumbnails'][0]['url'], 'https://cdn.example/poster.jpg')

    def test_an_image_item_without_video_is_left_out_like_the_built_in_extractor_does(self):
        reels = plugin.embedded_reels(page({'xdt_api__v1__feed__reels_media': {'reels_media': [
            reel('1111', 'someone', [image_item(IMAGE_PK)])]}}))
        self.assertIsNone(extractor()._story_result('someone', None, reels))

    def test_without_playlist_only_the_story_of_the_address_is_returned(self):
        result = extractor(noplaylist=True)._story_result(
            'someone', VIDEO_PK, plugin.embedded_reels(user_stories_page()))
        self.assertEqual(result['id'], _pk_to_id(VIDEO_PK))
        self.assertEqual(result['formats'][0]['url'], 'https://cdn.example/story.mp4')

    def test_a_highlight_is_found_in_the_connection_form(self):
        data = {'xdt_api__v1__feed__reels_media__connection': {'edges': [
            {'node': reel('highlight:999', 'someone', [video_item(OTHER_PK, 'https://cdn.example/other.mp4')])},
            {'node': reel('highlight:42', 'someone', [video_item(VIDEO_PK, 'https://cdn.example/h.mp4')],
                          title='Trips')},
        ]}}
        result = extractor()._story_result('highlights', '42', plugin.embedded_reels(page(data)))
        self.assertEqual(result['title'], 'Trips')
        self.assertEqual(result['id'], '42')
        self.assertEqual([e['formats'][0]['url'] for e in result['entries']], ['https://cdn.example/h.mp4'])

    def test_the_reel_holding_the_story_is_chosen_even_without_a_username(self):
        nameless = reel('1111', None, [video_item(VIDEO_PK, 'https://cdn.example/story.mp4')])
        del nameless['user']['username']
        decoy = reel('2222', 'someone', [video_item(OTHER_PK, 'https://cdn.example/other.mp4')])
        reels = plugin.embedded_reels(page({'xdt_api__v1__feed__reels_media': {'reels_media': [decoy, nameless]}}))

        result = extractor()._story_result('someone', VIDEO_PK, reels)
        self.assertEqual([e['formats'][0]['url'] for e in result['entries']], ['https://cdn.example/story.mp4'])

    def test_an_item_id_with_an_owner_suffix_identifies_the_story(self):
        item = video_item(None, 'https://cdn.example/story.mp4')
        del item['pk']
        item['id'] = f'{VIDEO_PK}_1111'
        holder = reel('1111', None, [item])
        decoy = reel('2222', 'someone', [video_item(OTHER_PK, 'https://cdn.example/other.mp4')])
        self.assertIs(plugin._InstagramStoryFallbackIE._select_reel('someone', VIDEO_PK, [decoy, holder]), holder)

    def test_an_item_whose_user_is_null_takes_the_reels_user(self):
        item = video_item(VIDEO_PK, 'https://cdn.example/story.mp4')
        item['user'] = None
        reels = plugin.embedded_reels(page({'xdt_api__v1__feed__reels_media': {'reels_media': [
            reel('1111', 'someone', [item])]}}))

        entry = extractor()._story_result('someone', None, reels)['entries'][0]
        self.assertEqual(entry['channel'], 'someone')
        self.assertEqual(entry['uploader'], 'Some One')

    def test_a_page_without_the_users_reel_gives_nothing(self):
        self.assertIsNone(extractor()._story_result('nobody', None, plugin.embedded_reels(user_stories_page())))
        self.assertEqual(list(plugin.embedded_reels('<html><script data-sjs>{not json</script></html>')), [])


class Fallback(unittest.TestCase):
    def run_with(self, built_in, webpage):
        ie = extractor()
        with mock.patch.object(BuiltIn, '_real_extract', built_in), \
                mock.patch.object(ie, '_download_webpage', webpage):
            return ie._real_extract(STORY_URL)

    def test_the_built_in_result_is_returned_untouched_when_it_works(self):
        found = {'id': 'from-built-in'}

        def unexpected_download(*args, **kwargs):
            raise AssertionError('the story page was downloaded again')

        self.assertIs(self.run_with(lambda self, url: found, unexpected_download), found)

    def test_another_error_of_the_built_in_extractor_propagates_unchanged(self):
        error = ExtractorError('Unable to extract user id')

        def failing(self, url):
            raise error

        # The page would answer the fallback, so only skipping it keeps the error.
        with self.assertRaises(ExtractorError) as raised:
            self.run_with(failing, lambda *args, **kwargs: user_stories_page())
        self.assertIs(raised.exception, error)

    def test_the_login_error_is_answered_from_the_embedded_reels(self):
        result = self.run_with(login_required, lambda *args, **kwargs: user_stories_page())
        self.assertEqual(result['_type'], 'playlist')
        self.assertEqual(result['id'], VIDEO_PK)
        self.assertEqual([e['formats'][0]['url'] for e in result['entries']], ['https://cdn.example/story.mp4'])

    def test_the_unreachable_error_is_answered_from_the_embedded_reels(self):
        def unreachable(self, url):
            self.raise_login_required('This content is unreachable')

        result = self.run_with(unreachable, lambda *args, **kwargs: user_stories_page())
        self.assertEqual([e['formats'][0]['url'] for e in result['entries']], ['https://cdn.example/story.mp4'])

    def test_the_original_login_error_is_raised_when_the_page_has_no_reels(self):
        with self.assertRaises(ExtractorError) as raised:
            self.run_with(login_required, lambda *args, **kwargs: '<html></html>')
        self.assertTrue(raised.exception.orig_msg.startswith('You need to log in to access this content'))

    def test_the_original_login_error_is_raised_when_the_page_cannot_be_downloaded(self):
        def refused(*args, **kwargs):
            raise ExtractorError('HTTP Error 500: Internal Server Error')

        with self.assertRaises(ExtractorError) as raised:
            self.run_with(login_required, refused)
        self.assertTrue(raised.exception.orig_msg.startswith('You need to log in to access this content'))


if __name__ == '__main__':
    unittest.main()
