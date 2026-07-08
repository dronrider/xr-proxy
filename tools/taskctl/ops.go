package main

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"time"
)

func boardPath(root string) string   { return filepath.Join(root, "docs", "TASKS.md") }
func archivePath(root string) string { return filepath.Join(root, "docs", "TASKS-archive.md") }

// findRoot ищет корень репозитория (директорию с docs/TASKS.md) вверх от start.
func findRoot(start string) (string, error) {
	dir, err := filepath.Abs(start)
	if err != nil {
		return "", err
	}
	for {
		if _, err := os.Stat(boardPath(dir)); err == nil {
			return dir, nil
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return "", fmt.Errorf("не нашёл docs/TASKS.md вверх от %s", start)
		}
		dir = parent
	}
}

func checkCell(name, s string) error {
	if strings.ContainsAny(s, "|\n") {
		return fmt.Errorf("%s не может содержать «|» и переводы строк", name)
	}
	return nil
}

func nextID(b *Board, a *Archive) (string, error) {
	prefix := ""
	max := 0
	scan := func(id string, num int) error {
		m := idRe.FindStringSubmatch(id)
		if prefix == "" {
			prefix = m[1]
		} else if prefix != m[1] {
			return fmt.Errorf("на доске и в архиве разные префиксы ID: %s и %s", prefix, m[1])
		}
		if num > max {
			max = num
		}
		return nil
	}
	for _, r := range b.Rows {
		if err := scan(r.ID, r.Num); err != nil {
			return "", err
		}
	}
	for _, r := range a.Rows {
		if err := scan(r.ID, r.Num); err != nil {
			return "", err
		}
	}
	if prefix == "" {
		return "", fmt.Errorf("ни одной задачи на доске и в архиве, укажи --id явно")
	}
	return fmt.Sprintf("%s-%03d", prefix, max+1), nil
}

type AddParams struct {
	ID, Title, Type, Rank, Link, Status, Reason string
}

func cmdAdd(root string, p AddParams) (string, error) {
	b, err := LoadBoard(boardPath(root))
	if err != nil {
		return "", err
	}
	arch, err := LoadArchive(archivePath(root))
	if err != nil {
		return "", err
	}
	id := p.ID
	if id == "" {
		if id, err = nextID(b, arch); err != nil {
			return "", err
		}
	} else if !idRe.MatchString(id) {
		return "", fmt.Errorf("ID %q не вида PREFIX-NNN", id)
	}
	if b.find(id) != nil || arch.has(id) {
		return "", fmt.Errorf("ID %s уже занят", id)
	}
	if strings.TrimSpace(p.Title) == "" {
		return "", fmt.Errorf("нужен --title")
	}
	if err := checkCell("заголовок", p.Title); err != nil {
		return "", err
	}
	if err := checkType(p.Type); err != nil {
		return "", err
	}
	total, parts, err := parseRank(p.Rank)
	if err != nil {
		return "", err
	}
	link := p.Link
	if link == "" {
		rel := fmt.Sprintf("tasks/%s.md", id)
		if _, err := os.Stat(filepath.Join(root, "docs", rel)); err != nil {
			return "", fmt.Errorf("файла docs/%s нет, укажи --link (или сначала создай файл задачи)", rel)
		}
		link = fmt.Sprintf("[%s](%s)", rel, rel)
	}
	if err := checkCell("ссылка", link); err != nil {
		return "", err
	}
	status := p.Status
	if status == "" {
		status = SectBacklog
	}
	sec, ok := b.Sects[status]
	if !ok {
		return "", fmt.Errorf("неизвестный статус %q, жду backlog / in-progress / check / blocked", status)
	}
	title := p.Title
	if status == SectBlocked && strings.TrimSpace(p.Reason) != "" {
		title += " [блок: " + p.Reason + "]"
	}
	row := &Row{ID: id, Num: mustNum(id), Title: title, Type: p.Type, P: bucket(total), RTotal: total, RParts: parts, Link: link}
	if err := insertRowLine(b, sec, row, formatRow(row)); err != nil {
		return "", err
	}
	if err := b.Save(); err != nil {
		return "", err
	}
	return fmt.Sprintf("%s заведена в %s: %s, R=%d", id, status, row.P, total), nil
}

func mustNum(id string) int {
	m := idRe.FindStringSubmatch(id)
	n := 0
	fmt.Sscanf(m[2], "%d", &n)
	return n
}

func cmdMove(root, id, target, reason string) (string, error) {
	b, err := LoadBoard(boardPath(root))
	if err != nil {
		return "", err
	}
	if _, ok := b.Sects[target]; !ok {
		return "", fmt.Errorf("неизвестный статус %q, жду backlog / in-progress / check / blocked", target)
	}
	row := b.find(id)
	if row == nil {
		return "", fmt.Errorf("%s нет на доске", id)
	}
	if row.Sect == target {
		return "", fmt.Errorf("%s уже в %s", id, target)
	}
	line := b.Lines[row.LineIdx]
	moved := *row
	if target == SectBlocked {
		if strings.TrimSpace(reason) == "" {
			return "", fmt.Errorf("для blocked обязателен --reason, одна строка почему")
		}
		if err := checkCell("причина", reason); err != nil {
			return "", err
		}
		moved.Title = row.Title + " [блок: " + reason + "]"
		line = formatRow(&moved)
	}
	b.remove(row.LineIdx)
	b2, err := parseLines(b.Path, b.Lines)
	if err != nil {
		return "", err
	}
	if err := insertRowLine(b2, b2.Sects[target], &moved, line); err != nil {
		return "", err
	}
	if err := b2.Save(); err != nil {
		return "", err
	}
	return fmt.Sprintf("%s: %s -> %s", id, row.Sect, target), nil
}

var commitRe = regexp.MustCompile(`^[0-9a-f]{7,40}$`)

type CloseParams struct {
	ID, Commits, Date, Link string
}

func cmdClose(root string, p CloseParams) (string, error) {
	b, err := LoadBoard(boardPath(root))
	if err != nil {
		return "", err
	}
	arch, err := LoadArchive(archivePath(root))
	if err != nil {
		return "", err
	}
	row := b.find(p.ID)
	if row == nil {
		return "", fmt.Errorf("%s нет на доске", p.ID)
	}
	if arch.has(p.ID) {
		return "", fmt.Errorf("%s уже есть в архиве", p.ID)
	}
	date := p.Date
	if date == "" {
		date = time.Now().Format("2006-01-02")
	}
	if !dateRe.MatchString(date) {
		return "", fmt.Errorf("дата %q не вида ГГГГ-ММ-ДД", date)
	}
	if _, err := time.Parse("2006-01-02", date); err != nil {
		return "", fmt.Errorf("дата %q не разбирается: %v", date, err)
	}
	var commits []string
	if p.Commits != "" {
		for _, c := range strings.Split(p.Commits, ",") {
			c = strings.TrimSpace(c)
			if !commitRe.MatchString(c) {
				return "", fmt.Errorf("%q не похоже на хеш коммита", c)
			}
			commits = append(commits, c)
		}
	}
	year := date[:4]
	moved := ""
	taskFile := filepath.Join(root, "docs", "tasks", p.ID+".md")
	if _, err := os.Stat(taskFile); err == nil {
		dst := filepath.Join(root, "docs", "tasks", "archive", year, p.ID+".md")
		if err := gitMv(root, taskFile, dst); err != nil {
			return "", err
		}
		moved = fmt.Sprintf("tasks/archive/%s/%s.md", year, p.ID)
	}
	linkCell := p.Link
	if linkCell == "" {
		var parts []string
		if moved != "" {
			parts = append(parts, fmt.Sprintf("[%s](%s)", moved, moved))
		}
		for _, c := range commits {
			parts = append(parts, "`"+c+"`")
		}
		if len(parts) == 0 {
			parts = append(parts, row.Link)
		}
		linkCell = strings.Join(parts, ", ")
	}
	if err := checkCell("ссылка", linkCell); err != nil {
		return "", err
	}
	cells := []string{p.ID, row.Title, row.Type, row.P, date, linkCell}
	if err := appendArchiveRow(archivePath(root), cells); err != nil {
		return "", err
	}
	b.remove(row.LineIdx)
	if err := b.Save(); err != nil {
		return "", err
	}
	msg := fmt.Sprintf("%s закрыта %s, строка в архиве", p.ID, date)
	if moved != "" {
		msg += ", файл задачи в " + moved
	}
	return msg, nil
}

// gitMv переносит файл через git mv, а вне git-репозитория (или для
// неотслеживаемого файла) обычным rename.
func gitMv(root, from, to string) error {
	if err := os.MkdirAll(filepath.Dir(to), 0o755); err != nil {
		return err
	}
	cmd := exec.Command("git", "-C", root, "mv", from, to)
	if out, err := cmd.CombinedOutput(); err != nil {
		if renameErr := os.Rename(from, to); renameErr != nil {
			return fmt.Errorf("git mv: %v (%s); rename: %v", err, strings.TrimSpace(string(out)), renameErr)
		}
	}
	return nil
}

func cmdSort(root string) (string, error) {
	b, err := LoadBoard(boardPath(root))
	if err != nil {
		return "", err
	}
	sec := b.Sects[SectBacklog]
	idxs := make([]int, len(sec.Rows))
	for i, r := range sec.Rows {
		idxs[i] = r.LineIdx
	}
	sorted := append([]*Row{}, sec.Rows...)
	sort.SliceStable(sorted, func(i, j int) bool {
		if sorted[i].RTotal != sorted[j].RTotal {
			return sorted[i].RTotal > sorted[j].RTotal
		}
		return sorted[i].Num < sorted[j].Num
	})
	contents := make([]string, len(sorted))
	for i, r := range sorted {
		contents[i] = b.Lines[r.LineIdx]
	}
	changed := 0
	for i := range idxs {
		if b.Lines[idxs[i]] != contents[i] {
			changed++
		}
		b.Lines[idxs[i]] = contents[i]
	}
	if changed == 0 {
		return "Backlog уже отсортирован", nil
	}
	if err := b.Save(); err != nil {
		return "", err
	}
	return fmt.Sprintf("Backlog пересортирован, строк переставлено: %d", changed), nil
}

func cmdID(root string) (string, error) {
	b, err := LoadBoard(boardPath(root))
	if err != nil {
		return "", err
	}
	arch, err := LoadArchive(archivePath(root))
	if err != nil {
		return "", err
	}
	return nextID(b, arch)
}
