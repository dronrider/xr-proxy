package main

import (
	"strings"
	"testing"
)

const fixtureBoard = `# Тест: доска (префикс XR)

Преамбула, утилита её не трогает.

## In progress

| ID | Задача | Тип | P | R | Ссылка |
|--------|--------|-----|---|---|--------|
| XR-005 | Задача в работе | task | P2 | 30 (25+2+1+0+2) | [tasks/XR-005.md](tasks/XR-005.md) |

## Check (готово, ждёт проверки пользователем)

| ID | Задача | Тип | P | R | Ссылка |
|--------|--------|-----|---|---|--------|

## Backlog

| ID | Задача | Тип | P | R | Ссылка |
|--------|--------|-----|---|---|--------|
| XR-002 | Верхняя | bug | P1 | 55 (50+0+0+5+0) | [tasks/XR-002.md](tasks/XR-002.md) |
| XR-001 | Средняя | task/LLD | P2 | 30 (25+2+1+0+2) | (LLD позже) |
| XR-003 | Та же R, больший ID | LLD | P2 | 30 (25+3+2+0+0) | (LLD позже) |
| XR-004 | Хвост | task | P3 | 9 (0+4+1+0+4) | (LLD позже) |

## Blocked

Нет.
`

const fixtureArchive = "# Тест: архив\n\n| ID | Задача | Тип | P | Закрыто | Ссылка |\n" +
	"|--------|--------|-----|---|---------|--------|\n" +
	"| XR-007 | Закрытая | bug | P1 | 2026-06-12 | `abc1234` |\n"

func TestRoundTrip(t *testing.T) {
	b, err := parseLines("board", strings.Split(fixtureBoard, "\n"))
	if err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(b.Lines, "\n"); got != fixtureBoard {
		t.Fatalf("разбор+сборка изменили файл:\n%s", got)
	}
	if len(b.Rows) != 5 {
		t.Fatalf("ожидал 5 строк, получил %d", len(b.Rows))
	}
	if b.Sects[SectBlocked].NetIdx == -1 {
		t.Fatal("не увидел «Нет.» в Blocked")
	}
}

func TestParseRowErrors(t *testing.T) {
	bad := []string{
		"| XR-001 | пять колонок | task | P3 | 9 (0+4+1+0+4) |",
		"| XR-001 | сумма не сходится | task | P3 | 10 (0+4+1+0+4) | x |",
		"| XR-001 | тип | feature | P3 | 9 (0+4+1+0+4) | x |",
		"| без-ид | ид | task | P3 | 9 (0+4+1+0+4) | x |",
		"| XR-001 | нет разбивки | task | P3 | 9 | x |",
	}
	for _, line := range bad {
		if _, err := parseBoardRow(line); err == nil {
			t.Errorf("ожидал ошибку на %q", line)
		}
	}
}

func TestParseSectionGarbage(t *testing.T) {
	board := strings.Replace(fixtureBoard, "Нет.", "произвольный текст", 1)
	if _, err := parseLines("board", strings.Split(board, "\n")); err == nil {
		t.Fatal("ожидал ошибку на постороннем тексте в секции")
	}
}

func TestBucket(t *testing.T) {
	cases := map[int]string{81: "P0", 75: "P0", 74: "P1", 50: "P1", 49: "P2", 25: "P2", 24: "P3", 0: "P3"}
	for r, want := range cases {
		if got := bucket(r); got != want {
			t.Errorf("bucket(%d) = %s, ожидал %s", r, got, want)
		}
	}
}

func TestParseRank(t *testing.T) {
	total, parts, err := parseRank("25+4+1+5+2")
	if err != nil || total != 37 || parts != [5]int{25, 4, 1, 5, 2} {
		t.Fatalf("parseRank: total=%d parts=%v err=%v", total, parts, err)
	}
	bad := []string{"1+2+3", "30+0+0+0+0", "0+11+0+0+0", "0+0+6+0+0", "0+0+0+3+0", "0+0+0+0+9", "а+0+0+0+0"}
	for _, s := range bad {
		if _, _, err := parseRank(s); err == nil {
			t.Errorf("ожидал ошибку на %q", s)
		}
	}
}
