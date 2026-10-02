package main

import "testing"

func TestValidateNameAndHerdrIDShapes(t *testing.T) {
	for _, name := range []string{"a", "worker-1", "sub_orch"} {
		if err := validateName(name, "name"); err != nil {
			t.Errorf("validateName(%q): %v", name, err)
		}
	}
	for _, name := range []string{"", "A", "a b", "1worker", "a/worker", "abcdefghijklmnopqrstuvwxyz0123456789"} {
		if err := validateName(name, "name"); err == nil {
			t.Errorf("validateName(%q) accepted", name)
		} else if e, ok := err.(*exitErr); !ok || e.code != exitUsage {
			t.Errorf("validateName(%q) = %v, want usage error", name, err)
		}
	}
	for _, id := range []string{"w1", "w1:t1", "w1:p1", "0", "a_b:c-d"} {
		if err := validateHerdrID(id, "--pane"); err != nil {
			t.Errorf("validateHerdrID(%q): %v", id, err)
		}
	}
	for _, id := range []string{"", " ", "null", "w1/p1", "w1.p1", "w1 p1"} {
		if err := validateHerdrID(id, "--pane"); err == nil {
			t.Errorf("validateHerdrID(%q) accepted", id)
		} else if e, ok := err.(*exitErr); !ok || e.code != exitUsage {
			t.Errorf("validateHerdrID(%q) = %v, want usage error", id, err)
		}
	}
}
