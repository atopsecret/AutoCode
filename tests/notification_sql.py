"""运行修复源码中的真实 SQL。所有 SQLite/plist 均为内存合成数据。

Run: python3 tests/notification_sql.py
Python 侧的排序/游标断言是算法模型检查，不代替 Rust/Tauri 集成测试。
"""
from pathlib import Path
import plistlib
import re
import sqlite3
import unittest

SOURCE = Path(__file__).resolve().parents[1] / "src-tauri/src/monitor/notification.rs"
SQL = re.search(r'"(SELECT r\.ROWID, a\.identifier, r\.uuid, r\.data, r\.delivered_date.*?)"', SOURCE.read_text(), re.S).group(1)


def database():
    db = sqlite3.connect(":memory:")
    db.executescript("CREATE TABLE app(app_id INTEGER PRIMARY KEY,identifier TEXT); CREATE TABLE record(app_id INTEGER,uuid BLOB,data BLOB,delivered_date REAL); INSERT INTO app VALUES(1,'com.synthetic.mail'); INSERT INTO app VALUES(2,'com.synthetic.chat');")
    return db


def insert(db, app=1, date=101.0, identity=b"synthetic-uuid", code="123456"):
    payload = plistlib.dumps({"req": {"titl": "验证", "body": f"验证码：{code}"}}, fmt=plistlib.FMT_BINARY)
    db.execute("INSERT INTO record VALUES(?,?,?,?)", (app, identity, payload, date))


def drain(db, high_water, since):
    rows = []
    after = 0
    while True:
        page = db.execute(SQL, (after, high_water, since, 200)).fetchall()
        if not page:
            return rows
        after = page[-1][0]
        rows.extend(page)


class NotificationSqlRegression(unittest.TestCase):
    def test_modern_schema_uses_app_join_and_data_plist(self):
        db = database()
        insert(db)
        row = drain(db, 0, 100)[0]
        self.assertEqual(row[1], "com.synthetic.mail")
        self.assertIn("123456", plistlib.loads(row[3])["req"]["body"])

    def test_numeric_app_identifier_is_resolved_not_dropped(self):
        db = database()
        insert(db, app=1)
        self.assertEqual(drain(db, 0, 100)[0][1], "com.synthetic.mail")

    def test_late_insert_behind_time_window_is_captured_by_rowid(self):
        db = database()
        insert(db, date=101)
        self.assertEqual(len(drain(db, 0, 200)), 1)

    def test_existing_row_delayed_fill_is_captured_in_overlap(self):
        db = database()
        insert(db, date=101)
        self.assertEqual(len(drain(db, 1, 100)), 1)

    def test_pages_drain_more_than_fifty_with_unrelated_apps(self):
        db = database()
        insert(db)
        for index in range(455):
            insert(db, app=2, date=102 + index, identity=str(index).encode())
        rows = drain(db, 0, 100)
        self.assertEqual(len(rows), 456)
        self.assertEqual(sum(row[1] == "com.synthetic.mail" for row in rows), 1)

    def test_sort_model_leaves_newest_code_last(self):
        db = database()
        insert(db, date=102, code="222222")
        insert(db, date=101, code="111111", identity=b"other")
        rows = sorted(drain(db, 0, 100), key=lambda row: (row[4], row[0]))
        self.assertIn("222222", plistlib.loads(rows[-1][3])["req"]["body"])

    def test_failed_query_preserves_model_high_water(self):
        db = database()
        high_water = 100
        db.execute("DROP TABLE record")
        with self.assertRaises(sqlite3.OperationalError):
            drain(db, high_water, 100)
        self.assertEqual(high_water, 100)

    def test_identical_text_has_independent_notification_identity(self):
        db = database()
        insert(db, identity=b"one")
        insert(db, identity=b"two")
        rows = drain(db, 0, 100)
        self.assertEqual(rows[0][3], rows[1][3])
        self.assertNotEqual(rows[0][2], rows[1][2])


if __name__ == "__main__":
    unittest.main(verbosity=2)
