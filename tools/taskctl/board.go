package main

import (
	"fmt"
	"os"
	"regexp"
	"strconv"
	"strings"
)

// Ключи секций доски. Заголовок матчится по префиксу, его текст в файле
// сохраняется как есть.
const (
	SectBacklog    = "backlog"
	SectInProgress = "in-progress"
	SectCheck      = "check"
	SectBlocked    = "blocked"
)

var sectByPrefix = []struct{ prefix, key string }{
	{"## In progress", SectInProgress},
	{"## Check", SectCheck},
	{"## Backlog", SectBacklog},
	{"## Blocked", SectBlocked},
}

const (
	tableHeader = "| ID | Задача | Тип | P | R | Ссылка |"
	tableSep    = "|--------|--------|-----|---|---|--------|"
)

// Row это разобранная строка таблицы доски. Пока строку не меняли, в файл
// уходит её исходный текст из Lines, а не пересборка из полей.
type Row struct {
	LineIdx int
	Sect    string
	ID      string
	Num     int
	Title   string
	Type    string
	P       string
	RTotal  int
	RParts  [5]int
	Link    string
}

type Section struct {
	Key        string
	HeadingIdx int
	HeaderIdx  int // строка шапки таблицы, -1 если таблицы нет
	SepIdx     int
	NetIdx     int // строка «Нет.» вместо таблицы, -1 если её нет
	Rows       []*Row
}

type Board struct {
	Path  string
	Lines []string
	Sects map[string]*Section
	Rows  []*Row
}

var (
	idRe   = regexp.MustCompile(`^([A-ZА-Я]+)-([0-9]+)$`)
	rRe    = regexp.MustCompile(`^([0-9]+) \(([0-9]+)\+([0-9]+)\+([0-9]+)\+([0-9]+)\+([0-9]+)\)$`)
	dateRe = regexp.MustCompile(`^[0-9]{4}-[0-9]{2}-[0-9]{2}$`)
)

func LoadBoard(path string) (*Board, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	return parseLines(path, strings.Split(string(data), "\n"))
}

func parseLines(path string, lines []string) (*Board, error) {
	b := &Board{Path: path, Lines: lines, Sects: map[string]*Section{}}
	var cur *Section
	for i, ln := range lines {
		if strings.HasPrefix(ln, "## ") {
			cur = nil
			for _, sp := range sectByPrefix {
				if strings.HasPrefix(ln, sp.prefix) {
					if _, dup := b.Sects[sp.key]; dup {
						return nil, fmt.Errorf("%s:%d: секция %q встречается дважды", path, i+1, sp.key)
					}
					cur = &Section{Key: sp.key, HeadingIdx: i, HeaderIdx: -1, SepIdx: -1, NetIdx: -1}
					b.Sects[sp.key] = cur
					break
				}
			}
			if cur == nil {
				return nil, fmt.Errorf("%s:%d: неизвестная секция %q", path, i+1, ln)
			}
			continue
		}
		if cur == nil {
			continue
		}
		trimmed := strings.TrimSpace(ln)
		switch {
		case trimmed == "":
		case trimmed == "Нет.":
			cur.NetIdx = i
		case strings.HasPrefix(trimmed, "|"):
			if cur.HeaderIdx == -1 {
				cur.HeaderIdx = i
				continue
			}
			if cur.SepIdx == -1 {
				if !strings.HasPrefix(trimmed, "|-") {
					return nil, fmt.Errorf("%s:%d: ожидался разделитель таблицы", path, i+1)
				}
				cur.SepIdx = i
				continue
			}
			row, err := parseBoardRow(ln)
			if err != nil {
				return nil, fmt.Errorf("%s:%d: %v", path, i+1, err)
			}
			row.LineIdx = i
			row.Sect = cur.Key
			cur.Rows = append(cur.Rows, row)
			b.Rows = append(b.Rows, row)
		default:
			return nil, fmt.Errorf("%s:%d: неожиданный текст в секции: %q", path, i+1, trimmed)
		}
	}
	for _, key := range []string{SectInProgress, SectCheck, SectBacklog, SectBlocked} {
		if b.Sects[key] == nil {
			return nil, fmt.Errorf("%s: не найдена секция %q", path, key)
		}
	}
	return b, nil
}

func (b *Board) Save() error {
	return os.WriteFile(b.Path, []byte(strings.Join(b.Lines, "\n")), 0o644)
}

func (b *Board) insert(idx int, lines ...string) {
	b.Lines = append(b.Lines[:idx], append(append([]string{}, lines...), b.Lines[idx:]...)...)
}

func (b *Board) remove(idx int) {
	b.Lines = append(b.Lines[:idx], b.Lines[idx+1:]...)
}

func (b *Board) find(id string) *Row {
	for _, r := range b.Rows {
		if r.ID == id {
			return r
		}
	}
	return nil
}

func splitCells(line string) ([]string, error) {
	s := strings.TrimSpace(line)
	if !strings.HasPrefix(s, "|") || !strings.HasSuffix(s, "|") {
		return nil, fmt.Errorf("строка таблицы не обрамлена «|»")
	}
	parts := strings.Split(s, "|")
	cells := parts[1 : len(parts)-1]
	for i := range cells {
		cells[i] = strings.TrimSpace(cells[i])
	}
	return cells, nil
}

func parseBoardRow(line string) (*Row, error) {
	cells, err := splitCells(line)
	if err != nil {
		return nil, err
	}
	if len(cells) != 6 {
		return nil, fmt.Errorf("ожидалось 6 колонок, получилось %d", len(cells))
	}
	m := idRe.FindStringSubmatch(cells[0])
	if m == nil {
		return nil, fmt.Errorf("не разобран ID %q", cells[0])
	}
	num, _ := strconv.Atoi(m[2])
	r := &Row{ID: cells[0], Num: num, Title: cells[1], Type: cells[2], P: cells[3], Link: cells[5]}
	if err := checkType(r.Type); err != nil {
		return nil, err
	}
	rm := rRe.FindStringSubmatch(cells[4])
	if rm == nil {
		return nil, fmt.Errorf("не разобрана колонка R %q, жду вид «N (а+б+в+г+д)»", cells[4])
	}
	r.RTotal, _ = strconv.Atoi(rm[1])
	sum := 0
	for i := 0; i < 5; i++ {
		v, _ := strconv.Atoi(rm[2+i])
		r.RParts[i] = v
		sum += v
	}
	if sum != r.RTotal {
		return nil, fmt.Errorf("R=%d не сходится с разбивкой, сумма слагаемых %d", r.RTotal, sum)
	}
	return r, nil
}

func checkType(t string) error {
	for _, part := range strings.Split(t, "/") {
		switch part {
		case "bug", "task", "LLD":
		default:
			return fmt.Errorf("неизвестный тип %q", t)
		}
	}
	return nil
}

func bucket(r int) string {
	switch {
	case r >= 75:
		return "P0"
	case r >= 50:
		return "P1"
	case r >= 25:
		return "P2"
	default:
		return "P3"
	}
}

func formatRow(r *Row) string {
	rcell := fmt.Sprintf("%d (%d+%d+%d+%d+%d)",
		r.RTotal, r.RParts[0], r.RParts[1], r.RParts[2], r.RParts[3], r.RParts[4])
	return fmt.Sprintf("| %s | %s | %s | %s | %s | %s |", r.ID, r.Title, r.Type, r.P, rcell, r.Link)
}

// parseRank разбирает разбивку ранга «а+б+в+г+д» и проверяет диапазоны
// слагаемых по RANKING.md.
func parseRank(s string) (int, [5]int, error) {
	var parts [5]int
	fields := strings.Split(s, "+")
	if len(fields) != 5 {
		return 0, parts, fmt.Errorf("в разбивке ранга должно быть 5 слагаемых, получилось %d", len(fields))
	}
	names := [5]string{"серьёзность", "ценность", "неопределённость", "поправка на баг", "рычаг"}
	max := [5]int{75, 10, 5, 5, 5}
	total := 0
	for i, f := range fields {
		v, err := strconv.Atoi(strings.TrimSpace(f))
		if err != nil || v < 0 || v > max[i] {
			return 0, parts, fmt.Errorf("%s: жду число 0..%d, получил %q", names[i], max[i], f)
		}
		parts[i] = v
		total += v
	}
	if parts[0]%25 != 0 {
		return 0, parts, fmt.Errorf("серьёзность берётся из {0, 25, 50, 75}, получил %d", parts[0])
	}
	if parts[3] != 0 && parts[3] != 5 {
		return 0, parts, fmt.Errorf("поправка на баг это 0 или 5, получил %d", parts[3])
	}
	return total, parts, nil
}

// insertIdx возвращает индекс строки, перед которой вставлять новую строку
// секции, либо -1, когда в секции нет таблицы (Blocked с «Нет.»).
func insertIdx(sec *Section, r *Row) int {
	if sec.Key == SectBacklog {
		for _, ex := range sec.Rows {
			if ex.RTotal < r.RTotal || (ex.RTotal == r.RTotal && ex.Num > r.Num) {
				return ex.LineIdx
			}
		}
	}
	if n := len(sec.Rows); n > 0 {
		return sec.Rows[n-1].LineIdx + 1
	}
	if sec.SepIdx != -1 {
		return sec.SepIdx + 1
	}
	return -1
}

// insertRowLine вставляет готовую строку таблицы в секцию, при необходимости
// разворачивая «Нет.» в таблицу.
func insertRowLine(b *Board, sec *Section, r *Row, line string) error {
	idx := insertIdx(sec, r)
	if idx != -1 {
		b.insert(idx, line)
		return nil
	}
	if sec.NetIdx == -1 {
		return fmt.Errorf("в секции %q нет ни таблицы, ни «Нет.», не понимаю, куда вставлять", sec.Key)
	}
	b.Lines[sec.NetIdx] = tableHeader
	b.insert(sec.NetIdx+1, tableSep, line)
	return nil
}
