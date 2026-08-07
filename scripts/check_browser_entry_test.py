#!/usr/bin/env python3
"""Тест на ws_accept: эталонный вектор из RFC 6455 (раздел 1.3), а не сверка
скрипта с самим собой. Сама ошибка (XR-268) была симметричной: скрипт с
неверным WS_GUID сверял ответ своего же эхо-сервиса и оставался зелёным,
потому что обе стороны ошибались одинаково. Такое ловит только внешняя
истина."""
import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location(
    "check_browser_entry", os.path.join(HERE, "check-browser-entry.py"))
cbe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cbe)


class TestWsAccept(unittest.TestCase):
    def test_rfc6455_sample(self):
        # Пример из самого RFC 6455, раздел 1.3.
        key = "dGhlIHNhbXBsZSBub25jZQ=="
        expected = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        self.assertEqual(cbe.ws_accept(key), expected)

    def test_guid_matches_rfc(self):
        self.assertEqual(cbe.WS_GUID, "258EAFA5-E914-47DA-95CA-C5AB0DC85B11")


if __name__ == "__main__":
    unittest.main()
